use std::env;
use std::fs;
use std::path::Path;
use std::time::Instant;

use aws_sdk_s3::Client as S3Client;
use ltsearch::embedding::{
    fixed_generator_from_env, provider_from_env_or_default, EmbeddingGenerator, EmbeddingProvider,
};
#[cfg(feature = "ltembed")]
use ltsearch::embedding::{ltembed_config_from_env, LTEmbedEmbeddingGenerator};
use ltsearch::index::{load_static_chunks_from_s3, StaticIndexBuilder, TurboBuildConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    config_path: String,
    output_dir: String,
}

fn parse_args<I, S>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut config_path = None;
    let mut output_dir = None;
    let mut iter = args.into_iter();
    let _binary = iter.next();

    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --config".to_string())?;
                config_path = Some(value.as_ref().to_string());
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output_dir = Some(value.as_ref().to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(CliArgs {
        config_path: config_path.ok_or_else(|| "missing required --config".to_string())?,
        output_dir: output_dir.ok_or_else(|| "missing required --output".to_string())?,
    })
}

async fn run(args: CliArgs) -> Result<String, String> {
    let config_text = fs::read_to_string(&args.config_path)
        .map_err(|error| format!("failed to read {}: {error}", args.config_path))?;
    let config: TurboBuildConfig = serde_json::from_str(&config_text)
        .map_err(|error| format!("failed to parse {}: {error}", args.config_path))?;

    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let s3 = s3_client_from_env(&sdk_config);
    let chunks = load_static_chunks_from_s3(&s3, &config.sources)
        .await
        .map_err(|error| error.to_string())?;

    let provider = provider_from_env_or_default(
        "LTSEARCH_BUILD_EMBEDDING_PROVIDER",
        EmbeddingProvider::Fixed,
    )
    .map_err(|error| error.to_string())?;
    let generator = build_embedding_generator(provider)?;

    let chunk_count = chunks.len();
    eprintln!(
        "static index builder: loading {} chunks with {:?} provider",
        chunk_count, provider
    );

    let embeddings = vec![None; chunks.len()];
    let started = Instant::now();
    let result = StaticIndexBuilder::<()>::new()
        .build(
            Path::new(&args.output_dir),
            &chunks,
            &embeddings,
            &generator,
        )
        .map_err(|error| error.to_string())?;

    let elapsed = started.elapsed();
    let dir = Path::new(&args.output_dir);
    let mut total_size: u64 = 0;
    for file_name in [
        "centroids.bin",
        "projection.bin",
        "turbo_static.bin",
        "turbo_static_meta.bin",
        "turbo_static_text.bin",
    ] {
        let path = dir.join(file_name);
        if let Ok(meta) = fs::metadata(&path) {
            let size = meta.len();
            total_size += size;
            eprintln!("  {file_name}: {} bytes", size);
        }
    }

    Ok(format!(
        "built {} static records (dim={}) in {:?}, total {} bytes",
        result.record_count, result.embedding_dim, elapsed, total_size
    ))
}

fn build_embedding_generator(
    provider: EmbeddingProvider,
) -> Result<Box<dyn EmbeddingGenerator>, String> {
    match provider {
        EmbeddingProvider::Fixed => fixed_generator_from_env(
            "LTSEARCH_BUILD_FIXED_EMBEDDING",
            Some("LTSEARCH_BUILD_EMBEDDING_DIM"),
        )
        .map(|generator| Box::new(generator) as Box<dyn EmbeddingGenerator>)
        .map_err(|error| error.to_string()),
        #[cfg(feature = "ltembed")]
        EmbeddingProvider::LTEmbed => ltembed_config_from_env(
            "LTSEARCH_BUILD_LTEMBED_MODEL_PATH",
            "LTSEARCH_BUILD_LTEMBED_CONFIG_PATH",
            "LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH",
            "LTSEARCH_BUILD_LTEMBED_POOLING",
            "LTSEARCH_BUILD_LTEMBED_PREFIX",
        )
        .map_err(|error| error.to_string())
        .and_then(|config| {
            LTEmbedEmbeddingGenerator::from_config(&config)
                .map(|generator| Box::new(generator) as Box<dyn EmbeddingGenerator>)
                .map_err(|error| error.to_string())
        }),
    }
}

fn s3_client_from_env(config: &aws_config::SdkConfig) -> S3Client {
    match env::var("AWS_ENDPOINT_URL_S3") {
        Ok(endpoint_url) => {
            let s3_config = aws_sdk_s3::config::Builder::from(config)
                .endpoint_url(endpoint_url)
                .force_path_style(true)
                .build();
            S3Client::from_conf(s3_config)
        }
        Err(_) => S3Client::new(config),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(env::args()).map_err(std::io::Error::other)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let summary = runtime.block_on(run(args)).map_err(std::io::Error::other)?;
    println!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parse_args_accepts_config_and_output_flags() {
        let parsed = parse_args([
            "turbo_index_builder",
            "--config",
            "/tmp/config.json",
            "--output",
            "/tmp/out",
        ])
        .unwrap();

        assert_eq!(parsed.config_path, "/tmp/config.json");
        assert_eq!(parsed.output_dir, "/tmp/out");
    }

    #[test]
    fn parse_args_rejects_missing_output_value() {
        let error = parse_args([
            "turbo_index_builder",
            "--config",
            "/tmp/config.json",
            "--output",
        ])
        .unwrap_err();

        assert!(error.contains("--output"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let error = parse_args([
            "turbo_index_builder",
            "--config",
            "/tmp/config.json",
            "--output",
            "/tmp/out",
            "--verbose",
        ])
        .unwrap_err();

        assert!(error.contains("--verbose"));
    }
}
