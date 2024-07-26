#![deny(unused_crate_dependencies)]

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use cairo_lang_starknet_classes::casm_contract_class::CasmContractClass;
use cairo_vm::{program_hash::compute_program_hash_chain, types::program::Program};
use dotenv::dotenv;
use serde::Deserialize;
use sqlx::Pool;
use sqlx::{postgres::PgPoolOptions, types::Uuid};
use starknet_crypto::FieldElement;
use std::{env, io::Cursor, sync::Arc};
use tokio_util::io::ReaderStream;

#[tokio::main]
async fn main() {
    // initialize tracing
    tracing_subscriber::fmt::init();

    // Load environment variables from .env file
    dotenv().ok();

    // Read database connection info from environment variables
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    // Connect to the database
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Failed to create pool.");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to run migrations");

    let db_pool = Arc::new(pool);

    // build our application with a route
    let app = Router::new()
        .route(
            "/get-program",
            get({
                let db_pool = Arc::clone(&db_pool);
                move |program| get_program(program, db_pool)
            }),
        )
        .route(
            "/upload-program",
            post({
                let db_pool = Arc::clone(&db_pool);
                move |multipart| upload_program(multipart, db_pool)
            }),
        )
        .layer(DefaultBodyLimit::disable());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn get_program(
    program: Query<GetProgram>,
    db_pool: Arc<Pool<sqlx::Postgres>>,
) -> Result<impl IntoResponse, StatusCode> {
    let program_hash = &program.program_hash;

    let row = sqlx::query!("SELECT code FROM programs WHERE hash = $1", program_hash)
        .fetch_one(&*db_pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let code = row.code;

    let stream = ReaderStream::new(Cursor::new(code));
    let body = Body::from_stream(stream);

    let response = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.json\"", program_hash),
        )
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(response)
}

#[derive(Deserialize)]
struct GetProgram {
    program_hash: String,
}

async fn upload_program(
    mut multipart: Multipart,
    db_pool: Arc<Pool<sqlx::Postgres>>,
) -> Result<String, StatusCode> {
    let mut version: i32 = 0;
    let mut program_data = None;
    #[allow(unused_assignments)]
    let mut program_hash_hex = String::new();

    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().unwrap_or_default().to_string();
        if name == "program" {
            let raw_data = field.bytes().await.unwrap();
            let compiler_version = get_compiler_version(raw_data.to_vec()).unwrap();
            println!("Compiler version: {}", compiler_version);
            version = compiler_version.split('.').collect::<Vec<&str>>()[0]
                .parse::<i32>()
                .unwrap();
            program_data = Some(raw_data);
        }
    }

    if let Some(data) = program_data {
        println!("Uploading program with version {}", version);
        if version == 2 {
            let casm: CasmContractClass = serde_json::from_slice(&data).unwrap();
            let program_hash = casm.compiled_class_hash();
            let convert = FieldElement::from_bytes_be(&program_hash.to_be_bytes()).unwrap();
            program_hash_hex = format!("{:#x}", convert);
            println!("Program hash: {}", program_hash_hex);

            let id = Uuid::from_bytes(uuid::Uuid::new_v4().to_bytes_le());

            let result = sqlx::query!(
                "INSERT INTO programs (id, hash, code, version) VALUES ($1, $2, $3, $4)",
                id,
                program_hash_hex,
                data.as_ref(),
                version
            )
            .execute(&*db_pool)
            .await;

            if result.is_err() {
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        } else if version == 0 {
            let program =
                Program::from_bytes(&data, Some("main")).expect("Could not load program.");
            let stripped_program = program.get_stripped_program().unwrap();
            let bootloader_version = 0;
            let program_hash = compute_program_hash_chain(&stripped_program, bootloader_version)
                .expect("Failed to compute program hash.");

            program_hash_hex = format!("{:#x}", program_hash);
            println!("Program Hash: {}", program_hash_hex);

            let id = Uuid::from_bytes(uuid::Uuid::new_v4().to_bytes_le());

            let result = sqlx::query!(
                "INSERT INTO programs (id, hash, code, version) VALUES ($1, $2, $3, $4)",
                id,
                program_hash_hex,
                data.as_ref(),
                version
            )
            .execute(&*db_pool)
            .await;

            if result.is_err() {
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        } else {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    Ok(program_hash_hex)
}

fn get_compiler_version(bytes: Vec<u8>) -> Result<String, Box<dyn std::error::Error>> {
    let json_str = String::from_utf8(bytes)?;

    // Parse the JSON string to a serde_json::Value
    let json_value: serde_json::Value = serde_json::from_str(&json_str)?;

    // Access the "compiler_version" field and extract its value
    if let Some(version) = json_value.get("compiler_version").and_then(|v| v.as_str()) {
        Ok(version.to_string())
    } else {
        Err("compiler_version field not found or not a uint".into())
    }
}
