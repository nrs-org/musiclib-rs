//! YouTube Data API quota counter.
//!
//! Counts outgoing requests to `https://www.googleapis.com/youtube/v3/*`. All
//! read endpoints used in this codebase (videos.list, channels.list,
//! playlists.list, playlistItems.list) cost 1 unit per call regardless of how
//! many IDs are passed, so the call count *is* the quota count.
//!
//! Positioned below the scheduler so retries are billed individually (matching
//! YouTube's accounting) and below the caches so cache hits don't count.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;

use crate::http::{BodyExtractorCow, Error, HttpClient, Request, Response};

const YOUTUBE_API_PREFIX: &str = "https://www.googleapis.com/youtube/v3/";

pub struct QuotaCounter {
    inner: Arc<dyn HttpClient>,
    youtube_count: Arc<AtomicU64>,
}

impl QuotaCounter {
    pub fn new(inner: Arc<dyn HttpClient>, youtube_count: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            youtube_count,
        })
    }
}

#[async_trait]
impl HttpClient for QuotaCounter {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        if req.url.starts_with(YOUTUBE_API_PREFIX) {
            self.youtube_count.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.make_request(req, body_extractor).await
    }
}

/// The YouTube quota day `t` falls in: the date in US Pacific time, when
/// YouTube resets the daily quota. Pacific time is UTC-8, or UTC-7 from the
/// second Sunday of March 02:00 PST to the first Sunday of November
/// 02:00 PDT (the US rules since 2007), worked out here so the ledger needs
/// no time zone database.
pub fn pacific_date(t: time::OffsetDateTime) -> time::Date {
    use time::{Date, Duration, Month, Time};

    let t = t.to_offset(time::UtcOffset::UTC);
    let first_sunday = |month: Month| {
        let first = Date::from_calendar_date(t.year(), month, 1).expect("valid date");
        let days_to_sunday = (7 - first.weekday().number_days_from_sunday()) % 7;
        first + Duration::days(days_to_sunday.into())
    };
    // 02:00 PST = 10:00 UTC; 02:00 PDT = 09:00 UTC.
    let dst_start = (first_sunday(Month::March) + Duration::weeks(1))
        .with_time(Time::from_hms(10, 0, 0).expect("valid time"))
        .assume_utc();
    let dst_end = first_sunday(Month::November)
        .with_time(Time::from_hms(9, 0, 0).expect("valid time"))
        .assume_utc();
    let offset_hours = if dst_start <= t && t < dst_end { 7 } else { 8 };
    (t - Duration::hours(offset_hours)).date()
}

/// Per-day YouTube quota use, kept in the HTTP cache DB so every run on the
/// same quota day (cron and manual) draws from one total.
///
/// It reads the run's `QuotaCounter` count and adds what it hasn't recorded
/// yet to the current day's row on each `flush`, so units land on the day
/// they were spent even across midnight.
pub struct QuotaLedger {
    db: sea_orm::DatabaseConnection,
    youtube_count: Arc<AtomicU64>,
    /// How much of `youtube_count` is already in the DB. Locked across the
    /// write so two flushes can't both add the same units.
    recorded: tokio::sync::Mutex<u64>,
}

impl QuotaLedger {
    /// `youtube_count` is the counter handed to `QuotaCounter`.
    pub async fn new(
        db: sea_orm::DatabaseConnection,
        youtube_count: Arc<AtomicU64>,
    ) -> Result<Self, sea_orm::DbErr> {
        use sea_orm::ConnectionTrait;
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS quota_usage (
                day TEXT PRIMARY KEY NOT NULL,
                youtube_units INTEGER NOT NULL
            )",
        )
        .await?;
        let recorded = youtube_count.load(Ordering::Relaxed);
        Ok(Self {
            db,
            youtube_count,
            recorded: tokio::sync::Mutex::new(recorded),
        })
    }

    /// Add the units spent since the last flush to today's row.
    pub async fn flush(&self) -> Result<(), sea_orm::DbErr> {
        use sea_orm::{ConnectionTrait, Statement};
        let mut recorded = self.recorded.lock().await;
        let now = self.youtube_count.load(Ordering::Relaxed);
        let delta = now.saturating_sub(*recorded);
        if delta == 0 {
            return Ok(());
        }
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "INSERT INTO quota_usage (day, youtube_units) VALUES (?, ?)
                 ON CONFLICT (day) DO UPDATE
                 SET youtube_units = youtube_units + excluded.youtube_units",
                [today().into(), (delta as i64).into()],
            ))
            .await?;
        *recorded = now;
        Ok(())
    }

    /// YouTube units used today by every run, this one included.
    pub async fn used_today(&self) -> Result<u64, sea_orm::DbErr> {
        use sea_orm::{ConnectionTrait, Statement};
        self.flush().await?;
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT youtube_units FROM quota_usage WHERE day = ?",
                [today().into()],
            ))
            .await?;
        Ok(match row {
            Some(row) => row.try_get::<i64>("", "youtube_units")?.max(0) as u64,
            None => 0,
        })
    }
}

fn today() -> String {
    pacific_date(time::OffsetDateTime::now_utc()).to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::QuotaCounter;
    use crate::http::{
        BodyExtractor, BodyExtractorCow, Error, HttpClient, Method, RawResponse, Request, Response,
        ResponseStatus,
    };

    struct OkClient;
    #[async_trait]
    impl HttpClient for OkClient {
        async fn make_request(
            &self,
            _req: Request,
            extractor: BodyExtractorCow<'static>,
        ) -> Result<Arc<Response>, Error> {
            let raw = RawResponse {
                status: ResponseStatus::OK,
                headers: Cow::Owned(Vec::new()),
                body: reqwest::Body::from(Bytes::from_static(b"")),
            };
            Ok(Arc::new(extractor.extract_response(raw).await?))
        }
    }

    fn req(url: &str) -> Request {
        Request {
            method: Method::GET,
            url: url.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn counts_youtube_api_calls() {
        let count = Arc::new(AtomicU64::new(0));
        let counter: Arc<dyn HttpClient> =
            QuotaCounter::new(Arc::new(OkClient), Arc::clone(&count));

        counter
            .get_bytes(req(
                "https://www.googleapis.com/youtube/v3/videos?id=A&part=snippet",
            ))
            .await
            .unwrap();
        counter
            .get_bytes(req("https://www.googleapis.com/youtube/v3/channels?id=X"))
            .await
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    /// 2026 DST runs from Sun 8 March 10:00 UTC to Sun 1 November 09:00 UTC.
    #[test]
    fn pacific_date_follows_us_dst() {
        use time::macros::datetime;
        let date = |t| super::pacific_date(t).to_string();
        // Winter, UTC-8: 07:59 UTC is still the previous day.
        assert_eq!(date(datetime!(2026-01-15 07:59 UTC)), "2026-01-14");
        assert_eq!(date(datetime!(2026-01-15 08:00 UTC)), "2026-01-15");
        // Summer, UTC-7.
        assert_eq!(date(datetime!(2026-07-15 06:59 UTC)), "2026-07-14");
        assert_eq!(date(datetime!(2026-07-15 07:00 UTC)), "2026-07-15");
        // Around the switches.
        assert_eq!(date(datetime!(2026-03-08 09:59 UTC)), "2026-03-08");
        assert_eq!(date(datetime!(2026-03-09 06:59 UTC)), "2026-03-08");
        assert_eq!(date(datetime!(2026-11-01 08:59 UTC)), "2026-11-01");
        assert_eq!(date(datetime!(2026-11-02 07:59 UTC)), "2026-11-01");
        assert_eq!(date(datetime!(2026-11-02 08:00 UTC)), "2026-11-02");
    }

    /// Units are added once per flush, and two ledgers on one DB (two runs on
    /// the same day) share the total.
    #[tokio::test]
    async fn ledger_accumulates_across_runs() {
        let db = crate::httpcache::db::connect("sqlite::memory:".into())
            .await
            .unwrap();
        let a_count = Arc::new(AtomicU64::new(0));
        let b_count = Arc::new(AtomicU64::new(0));
        let a = super::QuotaLedger::new(db.clone(), Arc::clone(&a_count))
            .await
            .unwrap();
        let b = super::QuotaLedger::new(db, Arc::clone(&b_count))
            .await
            .unwrap();

        a_count.fetch_add(3, Ordering::Relaxed);
        assert_eq!(a.used_today().await.unwrap(), 3);
        assert_eq!(a.used_today().await.unwrap(), 3);
        b_count.fetch_add(2, Ordering::Relaxed);
        a_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(b.used_today().await.unwrap(), 5);
        assert_eq!(a.used_today().await.unwrap(), 6);
    }

    #[tokio::test]
    async fn ignores_non_youtube_calls() {
        let count = Arc::new(AtomicU64::new(0));
        let counter: Arc<dyn HttpClient> =
            QuotaCounter::new(Arc::new(OkClient), Arc::clone(&count));

        counter
            .get_bytes(req("https://api.spotify.com/v1/tracks/X"))
            .await
            .unwrap();
        counter
            .get_bytes(req("https://musicbrainz.org/ws/2/recording/X"))
            .await
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}
