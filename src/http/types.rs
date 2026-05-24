use std::{any::Any, borrow::Cow, fmt::Debug, sync::Arc};

use bytes::Bytes;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::http::{BodyExtractor, BodyExtractorCow};

pub type Method = reqwest::Method;
pub type HeaderName = reqwest::header::HeaderName;
pub type HeaderValue = reqwest::header::HeaderValue;
pub type ResponseStatus = reqwest::StatusCode;

pub(crate) trait AnySerializable: Debug + erased_serde::Serialize + Send + Sync {
    fn as_any_ref(&self) -> &dyn Any;
    fn as_serialize_ref(&self) -> &dyn erased_serde::Serialize;
}
pub(crate) struct AnySerializableImpl<T: Serialize + Send + Sync + 'static>(pub T);
impl<T> Debug for AnySerializableImpl<T>
where
    T: Serialize + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let json =
            serde_json::to_string(&self.0).unwrap_or_else(|_| "<non-serializable>".to_string());
        write!(f, "AnySerializableImpl({json})")
    }
}
impl<T: Serialize + Send + Sync + 'static> Serialize for AnySerializableImpl<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}
impl<T: Serialize + Send + Sync + 'static> AnySerializable for AnySerializableImpl<T> {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn as_serialize_ref(&self) -> &dyn erased_serde::Serialize {
        self
    }
}

#[derive(Debug)]
pub enum ResponseBody {
    Bytes(Bytes),
    Text(String),
    Json(Arc<dyn AnySerializable>),
}

impl ResponseBody {
    pub fn to_bytes(&self) -> Result<Bytes, erased_serde::Error> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.clone()),
            Self::Text(text) => Ok(Bytes::copy_from_slice(text.as_bytes())),
            Self::Json(json) => {
                let mut output = Vec::<u8>::new();
                let mut serializer = serde_json::Serializer::new(&mut output);
                let mut serializer = <dyn erased_serde::Serializer>::erase(&mut serializer);
                json.as_serialize_ref().erased_serialize(&mut serializer)?;
                Ok(output.into())
            }
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes.as_ref()),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn as_json<T>(&self) -> Option<&T>
    where
        T: Any + Serialize + Send + Sync + 'static,
    {
        match self {
            Self::Json(json) => json
                .as_any_ref()
                .downcast_ref::<AnySerializableImpl<T>>()
                .map(|impl_| &impl_.0),
            _ => None,
        }
    }

    pub fn from_json<T>(value: T) -> Self
    where
        T: Serialize + Send + Sync + 'static,
    {
        Self::Json(Arc::new(AnySerializableImpl(value)))
    }
}

impl TryFrom<ResponseBody> for reqwest::Body {
    fn try_from(value: ResponseBody) -> Result<Self, Self::Error> {
        value.to_bytes().map(reqwest::Body::from)
    }

    type Error = erased_serde::Error;
}

pub enum ResponseBodyState {
    Extracted(ResponseBody),
    Unextracted(BodyExtractorCow<'static>, reqwest::Body),
}

impl Debug for ResponseBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extracted(body) => f.debug_tuple("Extracted").field(body).finish(),
            Self::Unextracted(_, _) => f
                .debug_tuple("Unextracted")
                .field(&"<body extractor + body stream>")
                .finish(),
        }
    }
}

#[derive(Debug)]
pub struct RawResponse<'a> {
    pub status: ResponseStatus,
    pub headers: Cow<'a, [(HeaderName, HeaderValue)]>,
    pub body: reqwest::Body,
}

#[derive(Debug)]
pub struct Response {
    pub status: ResponseStatus,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: RwLock<ResponseBodyState>,
}

impl Response {
    pub fn from(
        res: reqwest::Response,
        extractor: super::BodyExtractorCow<'static>,
    ) -> Result<Self, Error> {
        let status = res.status();
        let headers = res
            .headers()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let body = reqwest::Body::from(res);
        Ok(Self {
            status,
            headers,
            body: RwLock::new(ResponseBodyState::Unextracted(extractor, body)),
        })
    }

    pub async fn peek_body(&self) -> RwLockReadGuard<'_, ResponseBodyState> {
        self.body.read().await
    }

    async fn try_body(&self) -> Option<RwLockReadGuard<'_, ResponseBody>> {
        RwLockReadGuard::try_map(self.body.read().await, |state| match state {
            ResponseBodyState::Extracted(body) => Some(body),
            _ => None,
        })
        .ok()
    }

    async fn extract(&self) -> Result<(), Error> {
        let mut body_state = self.body.write().await;
        if let ResponseBodyState::Unextracted(extractor, body) = &mut *body_state {
            let extracted_body = extractor
                .extract(RawResponse {
                    status: self.status,
                    headers: Cow::Borrowed(&self.headers),
                    body: std::mem::take(body),
                })
                .await?;
            *body_state = ResponseBodyState::Extracted(extracted_body);
        }

        Ok(())
    }

    pub async fn body(&self) -> Result<RwLockReadGuard<'_, ResponseBody>, Error> {
        // try to get the body without locking for write first
        if let Some(body) = self.try_body().await {
            return Ok(body);
        }

        // initial check fails, try to extract the body
        // (note that race condition is prevented by the RwLock)
        self.extract().await?;

        // try to get the body again after extraction
        Ok(self
            .try_body()
            .await
            .expect("Body should be extracted successfully after extraction attempt (logic error)"))
    }

    pub fn error_for_status_ref(&self) -> Result<&Self, Error> {
        if self.status.is_success() {
            Ok(self)
        } else {
            Err(Error::HttpStatus(self.status))
        }
    }

    pub fn error_for_status(self) -> Result<Self, Error> {
        if self.status.is_success() {
            Ok(self)
        } else {
            Err(Error::HttpStatus(self.status))
        }
    }

    pub async fn body_to_bytes(&self) -> Result<Bytes, Error> {
        if let Ok(body) = self.bytes().await {
            return Ok(Bytes::copy_from_slice(&body));
        }

        let body = self.body().await?;
        body.to_bytes().map_err(Error::ErasedSerialization)
    }

    pub async fn bytes(&self) -> Result<RwLockReadGuard<'_, [u8]>, Error> {
        RwLockReadGuard::try_map(self.body().await?, |body| body.as_bytes())
            .map_err(|_| Error::WrongBodyType)
    }

    pub async fn text(&self) -> Result<RwLockReadGuard<'_, str>, Error> {
        RwLockReadGuard::try_map(self.body().await?, |body| body.as_text())
            .map_err(|_| Error::WrongBodyType)
    }

    pub async fn json<T>(&self) -> Result<RwLockReadGuard<'_, T>, Error>
    where
        T: Any + Serialize + Send + Sync + 'static,
    {
        RwLockReadGuard::try_map(self.body().await?, |body| body.as_json::<T>())
            .map_err(|_| Error::WrongBodyType)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Option<Bytes>,

    // cache HTTP client only
    pub cache_key: Option<String>,
    pub force_refetch: bool,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP cache error: {0}")]
    Cache(#[from] crate::httpcache::Error),
    #[error("Invalid cached type")]
    InvalidCachedType,
    #[error("Body extraction failed: {0}")]
    BodyExtract(#[from] BodyExtractError),
    #[error("Wrong body type")]
    WrongBodyType,
    #[error("Status code indicates error: {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("Serialization error (erased_serde): {0}")]
    ErasedSerialization(#[from] erased_serde::Error),
}

#[derive(Debug, Error)]
pub enum BodyExtractError {
    #[error("Failed to extract bytes body: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Failed to serialize JSON body: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait HasHeaders {
    fn headers(&self) -> &[(HeaderName, HeaderValue)];

    fn get_header(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers()
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

impl HasHeaders for Response {
    fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}

impl HasHeaders for Request {
    fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}

impl<'a> HasHeaders for RawResponse<'a> {
    fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}
