use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use data_types::{
    names::{org_and_bucket_to_database, OrgBucketMappingError},
    DatabaseName,
};
use dml::{DmlMeta, DmlWrite};
use hyper::{Body, Method, Request, Response, StatusCode};
use observability_deps::tracing::debug;
use serde::Deserialize;
use snafu::{OptionExt, ResultExt, Snafu};

use crate::influxdb_ioxd::{http::utils::parse_body, server_type::ServerType};

use super::{
    error::{HttpApiError, HttpApiErrorExt, HttpApiErrorSource},
    metrics::LineProtocolMetrics,
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Snafu)]
pub enum HttpWriteError {
    #[snafu(display("Internal error mapping org & bucket: {}", source))]
    BucketMappingError { source: OrgBucketMappingError },

    #[snafu(display(
        "Internal error writing points into org {}, bucket {}:  {}",
        org,
        bucket_name,
        source
    ))]
    WritingPoints {
        org: String,
        bucket_name: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Expected query string in request, but none was provided"))]
    ExpectedQueryString {},

    /// Error for when we could not parse the http query uri (e.g.
    /// `?foo=bar&bar=baz)`
    #[snafu(display("Invalid query string in HTTP URI '{}': {}", query_string, source))]
    InvalidQueryString {
        query_string: String,
        source: serde_urlencoded::de::Error,
    },

    #[snafu(display("Error reading request body as utf8: {}", source))]
    ReadingBodyAsUtf8 { source: std::str::Utf8Error },

    #[snafu(display("Error parsing line protocol: {}", source))]
    ParsingLineProtocol { source: mutable_batch_lp::Error },

    #[snafu(display("Database {} not found", db_name))]
    DatabaseNotFound { db_name: String },

    #[snafu(display("Cannot parse body: {}", source))]
    ParseBody {
        source: crate::influxdb_ioxd::http::utils::ParseBodyError,
    },
}

impl HttpApiErrorSource for HttpWriteError {
    fn to_http_api_error(&self) -> HttpApiError {
        match self {
            e @ Self::BucketMappingError { .. } => e.internal_error(),
            e @ Self::WritingPoints { .. } => e.internal_error(),
            e @ Self::ExpectedQueryString { .. } => e.invalid(),
            e @ Self::InvalidQueryString { .. } => e.invalid(),
            e @ Self::ReadingBodyAsUtf8 { .. } => e.invalid(),
            e @ Self::ParsingLineProtocol { .. } => e.invalid(),
            e @ Self::DatabaseNotFound { .. } => e.not_found(),
            Self::ParseBody { source } => source.to_http_api_error(),
        }
    }
}

/// Write error when calling the underlying server type.
#[derive(Debug, Snafu)]
pub enum InnerWriteError {
    #[snafu(display("Database {} not found", db_name))]
    NotFound { db_name: String },

    #[snafu(display("Error while writing: {}", source))]
    OtherError {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Contains a request or a response.
///
/// This is used to be able to consume a reqest and transform it into a response if routing was successfull.
pub enum RequestOrResponse {
    /// Request still there, wasn't routed.
    Request(Request<Body>),

    /// Request was consumed and transformed into a response object. Routing was successfull.
    Response(Response<Body>),
}

#[async_trait]
pub trait HttpDrivenWrite: ServerType {
    /// Routes HTTP write requests.
    ///
    /// Returns `RequestOrResponse::Response` if the request was routed,
    /// Returns `RequestOrResponse::Response` if the request did not match (and needs to be handled some other way)
    async fn route_write_http_request(
        &self,
        req: Request<Body>,
    ) -> Result<RequestOrResponse, HttpWriteError> {
        if (req.method() != Method::POST) || (req.uri().path() != "/api/v2/write") {
            return Ok(RequestOrResponse::Request(req));
        }

        let span_ctx = req.extensions().get().cloned();

        let max_request_size = self.max_request_size();
        let lp_metrics = self.lp_metrics();

        let query = req.uri().query().context(ExpectedQueryString)?;

        let write_info: WriteInfo =
            serde_urlencoded::from_str(query).context(InvalidQueryString {
                query_string: String::from(query),
            })?;

        let db_name = org_and_bucket_to_database(&write_info.org, &write_info.bucket)
            .context(BucketMappingError)?;

        let body = parse_body(req, max_request_size).await.context(ParseBody)?;

        let body = std::str::from_utf8(&body).context(ReadingBodyAsUtf8)?;

        // The time, in nanoseconds since the epoch, to assign to any points that don't
        // contain a timestamp
        let default_time = Utc::now().timestamp_nanos();

        let (tables, stats) = match mutable_batch_lp::lines_to_batches_stats(body, default_time) {
            Ok(x) => x,
            Err(mutable_batch_lp::Error::EmptyPayload) => {
                debug!("nothing to write");
                return Ok(RequestOrResponse::Response(
                    Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap(),
                ));
            }
            Err(source) => return Err(HttpWriteError::ParsingLineProtocol { source }),
        };

        debug!(
            num_lines=stats.num_lines,
            num_fields=stats.num_fields,
            body_size=body.len(),
            %db_name,
            org=%write_info.org,
            bucket=%write_info.bucket,
            "inserting lines into database",
        );

        let write = DmlWrite::new(tables, DmlMeta::unsequenced(span_ctx));

        match self.write(&db_name, write).await {
            Ok(_) => {
                lp_metrics.record_write(
                    &db_name,
                    stats.num_lines,
                    stats.num_fields,
                    body.len(),
                    true,
                );
                Ok(RequestOrResponse::Response(
                    Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .unwrap(),
                ))
            }
            Err(InnerWriteError::NotFound { .. }) => {
                debug!(%db_name, ?stats, "database not found");
                // Purposefully do not record ingest metrics
                Err(HttpWriteError::DatabaseNotFound {
                    db_name: db_name.to_string(),
                })
            }
            Err(InnerWriteError::OtherError { source }) => {
                debug!(e=%source, %db_name, ?stats, "error writing lines");
                lp_metrics.record_write(
                    &db_name,
                    stats.num_lines,
                    stats.num_fields,
                    body.len(),
                    false,
                );
                Err(HttpWriteError::WritingPoints {
                    org: write_info.org.clone(),
                    bucket_name: write_info.bucket.clone(),
                    source,
                })
            }
        }
    }

    /// Max request size.
    fn max_request_size(&self) -> usize;

    /// Line protocol metrics.
    fn lp_metrics(&self) -> Arc<LineProtocolMetrics>;

    /// Perform write.
    async fn write(
        &self,
        db_name: &DatabaseName<'_>,
        write: DmlWrite,
    ) -> Result<(), InnerWriteError>;
}

#[derive(Debug, Deserialize)]
/// Body of the request to the /write endpoint
pub struct WriteInfo {
    pub org: String,
    pub bucket: String,
}

#[cfg(test)]
pub mod test_utils {
    use dml::DmlWrite;
    use http::{header::CONTENT_ENCODING, StatusCode};
    use metric::{Attributes, DurationHistogram, Metric, U64Counter, U64Histogram};
    use mutable_batch_lp::lines_to_batches;
    use reqwest::Client;

    use crate::influxdb_ioxd::{
        http::test_utils::{check_response, TestServer},
        server_type::ServerType,
    };

    /// Assert that writes work.
    ///
    /// The database `bucket_name="MyBucket", org_name="MyOrg"` must exist for this test to work.
    ///
    /// Returns write that was generated. The caller MUST check that the write is actually present.
    pub async fn assert_write<T>(test_server: &TestServer<T>) -> DmlWrite
    where
        T: ServerType,
    {
        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000";

        // send write data
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body(lp_data)
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, Some("")).await;

        DmlWrite::new(lines_to_batches(lp_data, 0).unwrap(), Default::default())
    }

    /// Assert that GZIP-compressed writes work.
    ///
    /// The database `bucket_name="MyBucket", org_name="MyOrg"` must exist for this test to work.
    ///
    /// Returns write that was generated. The caller MUST check that the write is actually present.
    pub async fn assert_gzip_write<T>(test_server: &TestServer<T>) -> DmlWrite
    where
        T: ServerType,
    {
        let client = Client::new();
        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1617286224000000000";

        // send write data encoded with gzip
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .header(CONTENT_ENCODING, "gzip")
            .body(gzip_str(lp_data))
            .send()
            .await;

        check_response("gzip_write", response, StatusCode::NO_CONTENT, Some("")).await;

        DmlWrite::new(lines_to_batches(lp_data, 0).unwrap(), Default::default())
    }

    /// Assert that write to an invalid database behave as expected.
    pub async fn assert_write_to_invalid_database<T>(test_server: TestServer<T>)
    where
        T: ServerType,
    {
        let client = Client::new();

        let bucket_name = "NotMyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                test_server.url(),
                bucket_name,
                org_name
            ))
            .body("cpu bar=1 10")
            .send()
            .await;

        check_response(
            "write_to_invalid_databases",
            response,
            StatusCode::NOT_FOUND,
            Some(""),
        )
        .await;
    }

    /// Assert that write metrics work.
    ///
    /// The database `bucket_name="MyBucket", org_name="MyOrg"` must exist for this test to work.
    ///
    /// If `test_incompatible` is set this will test the ingestion of schema-incompatible data.
    pub async fn assert_write_metrics<T>(test_server: TestServer<T>, test_incompatible: bool)
    where
        T: ServerType,
    {
        let metric_registry = test_server.server_type().metric_registry();

        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1568756160";
        let incompatible_lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=\"incompatible\" 1568756170";

        // send good data
        let org_name = "MyOrg";
        let bucket_name = "MyBucket";
        let post_url = format!(
            "{}/api/v2/write?bucket={}&org={}",
            test_server.url(),
            bucket_name,
            org_name
        );
        client
            .post(&post_url)
            .body(lp_data)
            .send()
            .await
            .expect("sent data");

        // The request completed successfully
        let request_count = metric_registry
            .get_instrument::<Metric<U64Counter>>("http_requests")
            .unwrap();

        let request_count_ok = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        let request_count_client_error = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "client_error"),
            ]))
            .unwrap()
            .clone();

        let request_count_server_error = request_count
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "server_error"),
            ]))
            .unwrap()
            .clone();

        let request_duration_ok = metric_registry
            .get_instrument::<Metric<DurationHistogram>>("http_request_duration")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("path", "/api/v2/write"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        assert_eq!(request_duration_ok.fetch().sample_count(), 1);
        assert_eq!(request_count_ok.fetch(), 1);
        assert_eq!(request_count_client_error.fetch(), 0);
        assert_eq!(request_count_server_error.fetch(), 0);

        // A single successful point landed
        let ingest_lines = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_lines")
            .unwrap();

        let ingest_lines_ok = ingest_lines
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .clone();

        let ingest_lines_error = ingest_lines
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "error"),
            ]))
            .unwrap()
            .clone();

        assert_eq!(ingest_lines_ok.fetch(), 1);
        assert_eq!(ingest_lines_error.fetch(), 0);

        // Which consists of two fields
        let observation = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_fields")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation, 2);

        // Bytes of data were written
        let observation = metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_bytes")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation, 98);

        // Batch size distribution is measured
        let observation = metric_registry
            .get_instrument::<Metric<U64Histogram>>("ingest_batch_size_bytes")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "MyOrg_MyBucket"),
                ("status", "ok"),
            ]))
            .unwrap()
            .fetch();
        assert_eq!(observation.total, 98);
        assert_eq!(observation.buckets[0].count, 1);
        assert_eq!(observation.buckets[1].count, 0);

        // Write to a non-existent database
        client
            .post(&format!(
                "{}/api/v2/write?bucket=NotMyBucket&org=NotMyOrg",
                test_server.url(),
            ))
            .body(lp_data)
            .send()
            .await
            .unwrap();

        // An invalid database should not be reported as a new metric
        assert!(metric_registry
            .get_instrument::<Metric<U64Counter>>("ingest_lines")
            .unwrap()
            .get_observer(&Attributes::from(&[
                ("db_name", "NotMyOrg_NotMyBucket"),
                ("status", "error"),
            ]))
            .is_none());
        assert_eq!(ingest_lines_ok.fetch(), 1);
        assert_eq!(ingest_lines_error.fetch(), 0);

        // Perform an invalid write
        if test_incompatible {
            client
                .post(&post_url)
                .body(incompatible_lp_data)
                .send()
                .await
                .unwrap();

            // This currently results in an InternalServerError which is correctly recorded
            // as a server error, but this should probably be a BadRequest client error (#2538)
            assert_eq!(ingest_lines_ok.fetch(), 1);
            assert_eq!(ingest_lines_error.fetch(), 1);
            assert_eq!(request_duration_ok.fetch().sample_count(), 1);
            assert_eq!(request_count_ok.fetch(), 1);
            assert_eq!(request_count_client_error.fetch(), 0);
            assert_eq!(request_count_server_error.fetch(), 1);
        }
    }

    fn gzip_str(s: &str) -> Vec<u8> {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        write!(encoder, "{}", s).expect("writing into encoder");
        encoder.finish().expect("successfully encoding gzip data")
    }
}
