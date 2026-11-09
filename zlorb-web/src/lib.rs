mod template;

use askama::Template;
use axum::{
    Router,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
};
use zlorb_lib::{error::ZlorbError, load_config_from_toml};

use crate::template::Index;

const IP: &str = "0.0.0.0";
const PORT: &str = "3000";

#[tokio::main]
pub async fn run() -> Result<(), ZlorbError> {
    let app = Router::new()
        .route("/", get(get_root))
        .route("/healthz", get(get_health));

    let binding = format!("{IP}:{PORT}");
    let listener = tokio::net::TcpListener::bind(&binding)
        .await
        .map_err(ZlorbError::Io)?;
    println!("Listening on http://{}", binding);

    axum::serve(listener, app).await.map_err(ZlorbError::Io)
}

async fn get_health() -> &'static str {
    "zlorb-web ok\n"
}

async fn get_root() -> impl IntoResponse {
    let mut index = Index::new();
    let repos = match load_config_from_toml() {
        Ok(config) => config.repositories,
        Err(e) => {
            e.print();
            Vec::new()
        }
    };
    let mapped_repos: Vec<String> = repos.iter().map(|x| x.path.clone()).collect();

    index.repos = if mapped_repos.len() < 1 {
        None
    } else {
        Some(mapped_repos)
    };

    match index.render() {
        Ok(rendered) => Html(rendered).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not render the page: {e}"),
        )
            .into_response(),
    }
}
