//! Porter — standalone MCP gateway for CLI tools and MCP servers.
//!
//! Two subcommands:
//! - `porter serve`: Streamable HTTP MCP server exposing all configured tools
//! - `porter stdio`: STDIO transport for Claude Desktop and other STDIO-based MCP clients

use std::path::{Path, PathBuf};

use anyhow::Result;
use std::sync::Arc;

use axum::http::Request;
use axum::response::IntoResponse;
use axum::Router;
use clap::{Parser, Subcommand};
use nimbus_porter::{run_hot_reload, PorterConfig, PorterMcpServer, PorterRegistry};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as TowerServiceExt;
use tracing_subscriber::EnvFilter;

/// Porter — standalone MCP gateway for CLI tools and MCP servers.
#[derive(Parser)]
#[command(
    name = "porter",
    version,
    about = "Porter — standalone MCP gateway for CLI tools and MCP servers"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a Streamable HTTP MCP server exposing all configured tools
    Serve {
        /// Path to porter.toml config file [default: ./porter.toml or ~/.config/porter/porter.toml]
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// HTTP port to listen on [default: 3000, or [listen].port from config]
        #[arg(short, long)]
        port: Option<u16>,
        /// Bind address [default: 127.0.0.1, or [listen].host from config]
        #[arg(long)]
        host: Option<String>,
    },
    /// Bridge all configured tools over STDIO (for Claude Desktop, etc.)
    Stdio {
        /// Path to porter.toml config file [default: ./porter.toml or ~/.config/porter/porter.toml]
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with env filter (RUST_LOG controls verbosity)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    print_banner();

    let cli = Cli::parse();
    let cancel = CancellationToken::new();

    // Ctrl-C handler — cancels the root token for graceful shutdown
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Shutting down Porter...");
        cancel_for_signal.cancel();
    });

    match cli.command {
        Commands::Serve {
            config,
            port,
            host,
        } => {
            let config_path = resolve_config(config)?;
            run_serve(config_path, host, port, cancel).await?;
        }
        Commands::Stdio { config } => {
            let config = resolve_config(config)?;
            run_stdio(config, cancel).await?;
        }
    }

    Ok(())
}

/// Start a Streamable HTTP MCP server exposing all configured tools.
///
/// Loads porter.toml, builds PorterRegistry, wraps in PorterMcpServer,
/// spawns a hot-reload background task, then serves via StreamableHttpService + axum.
async fn run_serve(
    config_path: PathBuf,
    host_override: Option<String>,
    port_override: Option<u16>,
    cancel: CancellationToken,
) -> Result<()> {
    let config = load_config(&config_path).await?;

    let host = host_override.unwrap_or(config.listen.host.clone());
    let port = port_override.unwrap_or(config.listen.port);

    let registry = PorterRegistry::from_config(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build Porter registry: {}", e))?;

    let server = PorterMcpServer::new(registry);

    // Get handles for the hot-reload task
    let registry_handle = server.registry_handle();
    let peers_handle = server.peers_handle();

    // Spawn hot-reload background task — watches config file, swaps registry on change,
    // notifies connected MCP client peers of tools-list-changed
    tokio::spawn(run_hot_reload(
        config_path.clone(),
        registry_handle,
        peers_handle,
        cancel.child_token(),
    ));

    // Set up Streamable HTTP MCP service (same pattern as Navigator's run_navigator_http)
    let session_manager = Arc::new(LocalSessionManager::default());
    let http_config = StreamableHttpServerConfig {
        cancellation_token: cancel.clone(),
        ..Default::default()
    };
    let server_for_factory = server.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(server_for_factory.clone()),
        session_manager,
        http_config,
    );

    let app = Router::new().fallback(move |req: Request<axum::body::Body>| {
        let svc = mcp_service.clone();
        async move { svc.oneshot(req).await.unwrap().into_response() }
    });

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", addr, e))?;

    tracing::info!(host = %host, port = %port, "Porter HTTP server listening");
    tracing::info!("Connect your MCP client to http://{}:{}/mcp", host, port);

    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(|e| anyhow::anyhow!("Porter HTTP server error: {}", e))?;

    tracing::info!("Porter HTTP server stopped");
    Ok(())
}

/// Bridge all configured tools over STDIO for STDIO-based MCP clients.
///
/// Loads porter.toml, builds PorterRegistry, wraps in PorterMcpServer,
/// then serves over stdin/stdout using rmcp's serve_with_ct.
async fn run_stdio(config_path: PathBuf, cancel: CancellationToken) -> Result<()> {
    let config = load_config(&config_path).await?;
    let registry = PorterRegistry::from_config(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build Porter registry: {}", e))?;

    let server = PorterMcpServer::new(registry);

    // Use rmcp's STDIO transport (same pattern as Navigator's run_navigator_stdio)
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server
        .serve_with_ct(transport, cancel.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize Porter stdio transport: {:?}", e))?;

    tracing::info!("Porter stdio transport initialized, waiting for messages");

    tokio::select! {
        result = running.waiting() => {
            match result {
                Ok(reason) => {
                    tracing::info!(?reason, "Porter stdio transport completed");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Porter stdio transport error");
                    return Err(anyhow::anyhow!("Porter stdio transport error: {}", e));
                }
            }
        }
        _ = cancel.cancelled() => {
            tracing::info!("Porter stdio transport cancelled");
        }
    }

    Ok(())
}

/// Resolve config file path: explicit flag → ./porter.toml → ~/.config/porter/porter.toml.
fn resolve_config(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }

    let local = Path::new("porter.toml");
    if local.exists() {
        return Ok(local.to_path_buf());
    }

    if let Some(config_dir) = dirs::config_dir() {
        let xdg = config_dir.join("porter").join("porter.toml");
        if xdg.exists() {
            return Ok(xdg);
        }
    }

    Err(anyhow::anyhow!(
        "No porter.toml found. Searched ./porter.toml and ~/.config/porter/porter.toml. \
         Use --config to specify a path."
    ))
}

/// Load and parse a porter.toml config file.
async fn load_config(config_path: &PathBuf) -> Result<PorterConfig> {
    let content = tokio::fs::read_to_string(config_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read config file {:?}: {}", config_path, e))?;
    let config: PorterConfig = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file {:?}: {}", config_path, e))?;
    Ok(config)
}

/// Print the Porter startup banner with raised 3D ANSI block art to stderr.
///
/// Renders "PORTER" using half-block characters (▀▄█) for compact height with
/// per-pixel shading that simulates a raised/embossed effect lit from the
/// top-right. Respects NO_COLOR and skips output when stderr is not a terminal.
fn print_banner() {
    use std::fmt::Write;
    use std::io::IsTerminal;

    if !std::io::stderr().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        return;
    }

    // PORTER in 5-col × 5-row pixel font. '1' = filled, '0' = empty.
    let font: [&[&str]; 6] = [
        &["11110", "10010", "11110", "10000", "10000"], // P
        &["01110", "10001", "10001", "10001", "01110"], // O
        &["11110", "10010", "11110", "10100", "10010"], // R
        &["11111", "00100", "00100", "00100", "00100"], // T
        &["11111", "10000", "11110", "10000", "11111"], // E
        &["11110", "10010", "11110", "10100", "10010"], // R
    ];

    let letter_w = 5;
    let gap_px = 1;
    let rows = 5;
    let total_w = font.len() * letter_w + (font.len() - 1) * gap_px;

    // Build full pixel grid (all letters laid out with gaps)
    let mut grid = vec![vec![false; total_w]; rows];
    for row in 0..rows {
        let mut col = 0;
        for (li, letter) in font.iter().enumerate() {
            for c in 0..letter_w {
                grid[row][col] = letter[row].as_bytes()[c] == b'1';
                col += 1;
            }
            if li < font.len() - 1 {
                col += gap_px;
            }
        }
    }

    // Compute shade per pixel: light source at top-right.
    // Exposed top/right edges = highlight, exposed bottom/left edges = shadow.
    // Range: -2 (deep shadow) to +2 (bright highlight).
    let shade_at = |x: usize, y: usize| -> i8 {
        if !grid[y][x] {
            return 0;
        }
        let top = y == 0 || !grid[y - 1][x];
        let right = x + 1 >= total_w || !grid[y][x + 1];
        let bot = y + 1 >= rows || !grid[y + 1][x];
        let left = x == 0 || !grid[y][x - 1];
        (top as i8 + right as i8) - (bot as i8 + left as i8)
    };

    // Shade → color. Each shade level has (start, end) RGB for a subtle
    // left-to-right gradient. Lerp by x-position across the banner.
    #[rustfmt::skip]
    let shade_palette: [(f32,f32,f32, f32,f32,f32); 5] = [
        //  r0    g0    b0     r1    g1    b1         — red/umber/orange palette
        ( 45.0, 18.0, 10.0,  60.0, 25.0,  14.0), // shade -2: deep umber shadow
        ( 90.0, 35.0, 18.0, 115.0, 48.0,  25.0), // shade -1: dark rust shadow
        (155.0, 65.0, 28.0, 185.0, 85.0,  38.0), // shade  0: burnt orange face
        (210.0,120.0, 55.0, 235.0,150.0,  75.0), // shade +1: warm amber highlight
        (250.0,185.0,110.0, 255.0,215.0, 150.0), // shade +2: bright gold highlight
    ];

    let pixel_color = |x: usize, y: usize| -> Option<(u8, u8, u8)> {
        if !grid[y][x] {
            return None;
        }
        let s = shade_at(x, y);
        let idx = (s + 2).clamp(0, 4) as usize;
        let (r0, g0, b0, r1, g1, b1) = shade_palette[idx];
        let t = x as f32 / (total_w - 1) as f32;
        Some((
            (r0 + t * (r1 - r0)) as u8,
            (g0 + t * (g1 - g0)) as u8,
            (b0 + t * (b1 - b0)) as u8,
        ))
    };

    // Render using half-block characters for compact height (3 lines instead of 5).
    // Each terminal line encodes two pixel rows: top via foreground, bottom via background.
    let mut buf = String::with_capacity(4096);
    buf.push('\n');

    let row_pairs: [(usize, Option<usize>); 3] = [(0, Some(1)), (2, Some(3)), (4, None)];

    for &(top_row, bot_row) in &row_pairs {
        buf.push_str("    ");
        for x in 0..total_w {
            let top = pixel_color(x, top_row);
            let bot = bot_row.and_then(|by| pixel_color(x, by));

            match (top, bot) {
                (Some(tc), Some(bc)) => {
                    // ▀: fg paints upper half, bg paints lower half
                    let _ = write!(
                        buf,
                        "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀▀\x1b[0m",
                        tc.0, tc.1, tc.2, bc.0, bc.1, bc.2
                    );
                }
                (Some(tc), None) => {
                    let _ = write!(
                        buf,
                        "\x1b[38;2;{};{};{}m▀▀\x1b[0m",
                        tc.0, tc.1, tc.2
                    );
                }
                (None, Some(bc)) => {
                    let _ = write!(
                        buf,
                        "\x1b[38;2;{};{};{}m▄▄\x1b[0m",
                        bc.0, bc.1, bc.2
                    );
                }
                (None, None) => {
                    buf.push_str("  ");
                }
            }
        }
        buf.push('\n');
    }

    buf.push_str(&format!(
        "    \x1b[2;38;2;180;110;60mv{}  ·  Part of the Nimbus Ecosystem\x1b[0m\n\n",
        env!("CARGO_PKG_VERSION")
    ));

    eprint!("{buf}");
}
