use crate::XiaoaiResponse;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("API 返回 {}: {}", .0.code, .0.message)]
    Api(XiaoaiResponse),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Cookie(#[from] cookie_store::CookieError),

    #[error(transparent)]
    Url(#[from] url::ParseError),
}
