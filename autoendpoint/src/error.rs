//! Error types and transformations

use crate::db::error::DbError;
use crate::headers::vapid::VapidError;
use crate::routers::RouterError;
use actix_web::{
    dev::{HttpResponseBuilder, ServiceResponse},
    error::{PayloadError, ResponseError},
    http::StatusCode,
    middleware::errhandlers::ErrorHandlerResponse,
    HttpResponse, Result,
};
use backtrace::Backtrace;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::error::Error;
use std::fmt::{self, Display};
use thiserror::Error;
use validator::{ValidationErrors, ValidationErrorsKind};

/// Common `Result` type.
pub type ApiResult<T> = Result<T, ApiError>;

/// A link for more info on the returned error
const ERROR_URL: &str = "http://autopush.readthedocs.io/en/latest/http.html#error-codes";

/// The main error type.
#[derive(Debug)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub backtrace: Backtrace,
}

impl ApiError {
    /// Render a 404 response
    pub fn render_404<B>(res: ServiceResponse<B>) -> Result<ErrorHandlerResponse<B>> {
        // Replace the outbound error message with our own.
        let resp = HttpResponseBuilder::new(StatusCode::NOT_FOUND).finish();
        Ok(ErrorHandlerResponse::Response(ServiceResponse::new(
            res.request().clone(),
            resp.into_body(),
        )))
    }
}

/// The possible errors this application could encounter
#[derive(Debug, Error)]
pub enum ApiErrorKind {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Metrics(#[from] cadence::MetricError),

    #[error(transparent)]
    Validation(#[from] validator::ValidationErrors),

    // PayloadError does not implement std Error
    #[error("{0}")]
    PayloadError(PayloadError),

    #[error(transparent)]
    VapidError(#[from] VapidError),

    #[error(transparent)]
    Router(#[from] RouterError),

    #[error(transparent)]
    Jwt(#[from] jsonwebtoken::errors::Error),

    #[error("Error while validating token")]
    TokenHashValidation(#[source] openssl::error::ErrorStack),

    #[error("Database error: {0}")]
    Database(#[from] DbError),

    #[error("Invalid token")]
    InvalidToken,

    #[error("UAID not found")]
    NoUser,

    #[error("No such subscription")]
    NoSubscription,

    /// A specific issue with the encryption headers
    #[error("{0}")]
    InvalidEncryption(String),

    #[error("Data payload must be smaller than {0} bytes")]
    PayloadTooLarge(usize),

    /// Used if the API version given is not v1 or v2
    #[error("Invalid API version")]
    InvalidApiVersion,

    #[error("Missing TTL value")]
    NoTTL,

    #[error("{0}")]
    Internal(String),
}

impl ApiErrorKind {
    /// Get the associated HTTP status code
    pub fn status(&self) -> StatusCode {
        match self {
            ApiErrorKind::PayloadError(e) => e.status_code(),
            ApiErrorKind::Router(e) => e.status(),

            ApiErrorKind::Validation(_)
            | ApiErrorKind::InvalidEncryption(_)
            | ApiErrorKind::TokenHashValidation(_)
            | ApiErrorKind::NoTTL => StatusCode::BAD_REQUEST,

            ApiErrorKind::NoUser | ApiErrorKind::NoSubscription => StatusCode::GONE,

            ApiErrorKind::VapidError(_) | ApiErrorKind::Jwt(_) => StatusCode::UNAUTHORIZED,

            ApiErrorKind::InvalidToken | ApiErrorKind::InvalidApiVersion => StatusCode::NOT_FOUND,

            ApiErrorKind::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,

            ApiErrorKind::Io(_)
            | ApiErrorKind::Metrics(_)
            | ApiErrorKind::Database(_)
            | ApiErrorKind::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Get the associated error number
    pub fn errno(&self) -> Option<usize> {
        match self {
            ApiErrorKind::Router(e) => e.errno(),

            ApiErrorKind::Validation(e) => errno_from_validation_errors(e),

            ApiErrorKind::InvalidToken | ApiErrorKind::InvalidApiVersion => Some(102),

            ApiErrorKind::NoUser => Some(103),

            ApiErrorKind::PayloadError(PayloadError::Overflow)
            | ApiErrorKind::PayloadTooLarge(_) => Some(104),

            ApiErrorKind::NoSubscription => Some(106),

            ApiErrorKind::VapidError(_)
            | ApiErrorKind::TokenHashValidation(_)
            | ApiErrorKind::Jwt(_) => Some(109),

            ApiErrorKind::InvalidEncryption(_) => Some(110),

            ApiErrorKind::NoTTL => Some(111),

            ApiErrorKind::Internal(_) => Some(999),

            ApiErrorKind::Io(_)
            | ApiErrorKind::Metrics(_)
            | ApiErrorKind::Database(_)
            | ApiErrorKind::PayloadError(_) => None,
        }
    }
}

// Print out the error and backtrace, including source errors
impl Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error: {}\nBacktrace: \n{:?}", self.kind, self.backtrace)?;

        // Go down the chain of errors
        let mut error: &dyn Error = &self.kind;
        while let Some(source) = error.source() {
            write!(f, "\n\nCaused by: {}", source)?;
            error = source;
        }

        Ok(())
    }
}

impl Error for ApiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.kind.source()
    }
}

// Forward From impls to ApiError from ApiErrorKind. Because From is reflexive,
// this impl also takes care of From<ApiErrorKind>.
impl<T> From<T> for ApiError
where
    ApiErrorKind: From<T>,
{
    fn from(item: T) -> Self {
        ApiError {
            kind: ApiErrorKind::from(item),
            backtrace: Backtrace::new(),
        }
    }
}

impl From<actix_web::error::BlockingError<ApiError>> for ApiError {
    fn from(inner: actix_web::error::BlockingError<ApiError>) -> Self {
        match inner {
            actix_web::error::BlockingError::Error(e) => e,
            actix_web::error::BlockingError::Canceled => {
                ApiErrorKind::Internal("Db threadpool operation canceled".to_owned()).into()
            }
        }
    }
}

impl From<ApiError> for HttpResponse {
    fn from(inner: ApiError) -> Self {
        ResponseError::error_response(&inner)
    }
}

impl ResponseError for ApiError {
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.kind.status()).json(self)
    }
}

impl Serialize for ApiError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let status = self.kind.status();
        let mut map = serializer.serialize_map(Some(5))?;

        map.serialize_entry("code", &status.as_u16())?;
        map.serialize_entry("errno", &self.kind.errno())?;
        map.serialize_entry("error", &status.canonical_reason())?;
        map.serialize_entry("message", &self.kind.to_string())?;
        map.serialize_entry("more_info", ERROR_URL)?;
        map.end()
    }
}

/// Get the error number from validation errors. If multiple errors are present,
/// the first one with a valid error code is used.
fn errno_from_validation_errors(e: &ValidationErrors) -> Option<usize> {
    // Build an iterator over the error numbers, then get the first one
    e.errors()
        .values()
        .flat_map(|error| match error {
            ValidationErrorsKind::Struct(inner_errors) => {
                Box::new(errno_from_validation_errors(inner_errors).into_iter())
                    as Box<dyn Iterator<Item = usize>>
            }
            ValidationErrorsKind::List(indexed_errors) => Box::new(
                indexed_errors
                    .values()
                    .filter_map(|errors| errno_from_validation_errors(errors)),
            )
                as Box<dyn Iterator<Item = usize>>,
            ValidationErrorsKind::Field(errors) => {
                Box::new(errors.iter().filter_map(|error| error.code.parse().ok()))
                    as Box<dyn Iterator<Item = usize>>
            }
        })
        .next()
}