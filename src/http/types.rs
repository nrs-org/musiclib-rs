use std::{any::Any, fmt::Debug, sync::Arc};

use serde::Serialize;
use thiserror::Error;

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

#[derive(Debug, Clone)]
pub enum ResponseBodyInner {
    Bytes(Vec<u8>),
    Text(String),
    Json(Arc<dyn AnySerializable>),
}

#[derive(Debug, Clone)]
pub struct ResponseBody {
    inner: ResponseBodyInner,
}

impl ResponseBody {
    pub fn to_bytes(&self) -> Result<Vec<u8>, erased_serde::Error> {
        match &self.inner {
            ResponseBodyInner::Bytes(bytes) => Ok(bytes.clone()),
            ResponseBodyInner::Text(text) => Ok(text.as_bytes().to_vec()),
            ResponseBodyInner::Json(json) => {
                let mut output = Vec::<u8>::new();
                let mut serializer = serde_json::Serializer::new(&mut output);
                let mut serializer = <dyn erased_serde::Serializer>::erase(&mut serializer);
                json.as_serialize_ref().erased_serialize(&mut serializer)?;
                Ok(output)
            }
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match &self.inner {
            ResponseBodyInner::Text(text) => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn as_json<T>(&self) -> Option<&T>
    where
        T: Any + Serialize + Send + Sync + 'static,
    {
        match &self.inner {
            ResponseBodyInner::Json(json) => json
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
        Self {
            inner: ResponseBodyInner::Json(Arc::new(AnySerializableImpl(value))),
        }
    }
}

impl TryFrom<ResponseBody> for reqwest::Body {
    fn try_from(value: ResponseBody) -> Result<Self, Self::Error> {
        value.to_bytes().map(reqwest::Body::from)
    }

    type Error = erased_serde::Error;
}

impl From<Vec<u8>> for ResponseBody {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            inner: ResponseBodyInner::Bytes(bytes),
        }
    }
}

impl From<String> for ResponseBody {
    fn from(text: String) -> Self {
        Self {
            inner: ResponseBodyInner::Text(text),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: ResponseStatus,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: ResponseBody,
}

impl Response {
    pub async fn from(
        res: reqwest::Response,
        extractor: &dyn super::BodyExtractor,
    ) -> Result<Self, Error> {
        let status = res.status();
        let headers = res
            .headers()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let body = extractor.extract(res).await?;
        Ok(Self {
            status,
            headers,
            body,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Option<Vec<u8>>,

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
}

#[derive(Debug, Error)]
pub enum BodyExtractError {
    #[error("Failed to extract bytes body: {0}")]
    Http(#[from] reqwest::Error),
}
