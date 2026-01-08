use palc::Parser;
use regex::RegexSet;
use reqwest::{Client, Proxy};
use serde::Deserialize;
use std::fs;
use std::time::{Duration, Instant};
use tokio::time::timeout;

#[derive(Parser)]
#[command(name = "singbox-tester")]
#[command(long_about = "Test SingBox proxy nodes latency")]
struct Args {
    /// Path to the SingBox config JSON file
    config_path: String,
    /// Regex pattern to filter node tags (optional)
    ///
    /// Pass multiple patterns to match ALL of them.
    regexes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    inbounds: Option<Vec<Inbound>>,
}

#[derive(Debug, Deserialize)]
struct Inbound {
    #[serde(rename = "type")]
    inbound_type: Option<String>,
    tag: Option<String>,
    listen_port: Option<u16>,
    listen: Option<String>,
}

#[derive(Debug, Clone)]
enum LatencyResult {
    Success {
        median: f64,
        average: f64,
        minimum: f64,
        maximum: f64,
    },
    Unstable(usize, usize), // valid_count, total_count
    AllFailed,
    SessionError(String),
}

impl std::fmt::Display for LatencyResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LatencyResult::Success {
                median,
                average,
                maximum,
                minimum,
            } => write!(f, "{median:.2}/{average:.2}/{minimum:.2}/{maximum:.2}"),
            LatencyResult::Unstable(valid, total) => write!(f, "Unstable ({}/{})", valid, total),
            LatencyResult::AllFailed => write!(f, "All Failed"),
            LatencyResult::SessionError(err) => write!(f, "Session Error: {}", err),
        }
    }
}

async fn test_node_latency(port: u16, test_count: usize) -> LatencyResult {
    let url = "https://www.cloudflare.com/cdn-cgi/trace";
    let proxy_url = format!("socks5h://127.0.0.1:{}", port);

    // Create client with explicit timeouts
    let proxy = match Proxy::all(&proxy_url) {
        Ok(proxy) => proxy,
        Err(e) => return LatencyResult::SessionError(format!("Failed to create proxy: {}", e)),
    };

    let client = Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(10)) // Overall request timeout
        .connect_timeout(Duration::from_secs(5)) // Connection timeout
        .build();

    let client = match client {
        Ok(client) => client,
        Err(e) => return LatencyResult::SessionError(format!("Failed to create client: {}", e)),
    };

    let mut latencies = Vec::new();

    // Warmup request (don't count in statistics)
    println!("  预热连接...");
    let warmup_timeout = Duration::from_secs(10);
    let _ = timeout(warmup_timeout, client.head(url).send()).await;

    // Perform test requests
    for i in 0..test_count {
        let start = Instant::now();

        // Use explicit timeout for each request
        let request_timeout = Duration::from_secs(10);
        let result = timeout(request_timeout, client.head(url).send()).await;

        match result {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    let elapsed_ms = start.elapsed().as_micros() as f64 / 1000.;
                    latencies.push(elapsed_ms);
                    println!("  ↳ 第 {:2} 次: {:6.2} ms", i + 1, elapsed_ms);
                } else {
                    latencies.push(f64::INFINITY);
                    println!("  ↳ 第 {:2} 次: HTTP Error {}", i + 1, response.status());
                    break;
                }
            }
            Ok(Err(e)) => {
                latencies.push(f64::INFINITY);
                if e.is_connect() {
                    println!("  ↳ 第 {:2} 次: Connect Error: {e}", i + 1);
                } else if e.is_timeout() {
                    println!("  ↳ 第 {:2} 次: Request Timeout: {e}", i + 1);
                } else {
                    let error_str = format!("{e}");
                    let truncated = if error_str.len() > 20 {
                        &error_str[..20]
                    } else {
                        &error_str
                    };
                    println!("  ↳ 第 {:2} 次: Error ({})", i + 1, truncated);
                }
                break;
            }
            Err(_) => {
                // Timeout occurred
                latencies.push(f64::INFINITY);
                println!("  ↳ 第 {:2} 次: Timeout", i + 1);
                break;
            }
        }
    }

    // Process results
    if latencies.is_empty() || latencies.iter().all(|&l| l.is_infinite()) {
        return LatencyResult::AllFailed;
    }

    let valid_latencies: Vec<f64> = latencies
        .into_iter()
        .filter(|&l| !l.is_infinite())
        .collect();

    if valid_latencies.len() < 3 {
        return LatencyResult::Unstable(valid_latencies.len(), test_count);
    }

    // Calculate median
    let mut sorted = valid_latencies;
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let average = sorted.iter().sum::<f64>() / sorted.len() as f64;

    LatencyResult::Success {
        median,
        average,
        minimum: *sorted.first().unwrap(),
        maximum: *sorted.last().unwrap(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        config_path,
        regexes,
    } = Args::parse();

    let tag_filter = RegexSet::new(&regexes)?;

    // Read and parse config file
    let config_content = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("❌ 无法读取 JSON 文件: {}", e);
            return Ok(());
        }
    };

    let config: Config = match serde_json::from_str(&config_content) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("❌ JSON 解析失败: {}", e);
            return Ok(());
        }
    };

    let inbounds = match config.inbounds {
        Some(inbounds) => inbounds,
        None => {
            eprintln!("❌ 未找到 inbounds 字段");
            return Ok(());
        }
    };

    // Extract SOCKS nodes
    let mut socks_nodes = Vec::new();
    for inbound in inbounds {
        if let (Some(inbound_type), Some(tag), Some(port), listen) = (
            inbound.inbound_type,
            inbound.tag,
            inbound.listen_port,
            inbound.listen,
        ) {
            if inbound_type == "socks" {
                let listen_addr = listen.unwrap_or_else(|| "127.0.0.1".to_string());
                if tag_filter.matches(&tag).matched_all()
                    && matches!(listen_addr.as_str(), "127.0.0.1" | "::1" | "localhost")
                {
                    socks_nodes.push((tag, port));
                }
            }
        }
    }

    if socks_nodes.is_empty() {
        eprintln!("未找到任何 socks 类型 inbound");
        return Ok(());
    }

    println!(
        "找到 {} 个 socks 节点，开始顺序测试（每节点10次）\n",
        socks_nodes.len()
    );

    // Test nodes sequentially
    let mut results = Vec::new();
    for (idx, (tag, port)) in socks_nodes.iter().enumerate() {
        println!(
            "[{}/{}] 测试节点: {}  (端口: {})",
            idx + 1,
            socks_nodes.len(),
            tag,
            port
        );
        let latency = test_node_latency(*port, 10).await;
        results.push((tag.clone(), *port, latency.clone()));
        println!("  → 最终延迟: {}\n", latency);
    }

    // Sort results: successful nodes by latency ascending, failed nodes at the end
    results.sort_by(|a, b| match (&a.2, &b.2) {
        (
            LatencyResult::Success {
                median: median_a, ..
            },
            LatencyResult::Success {
                median: mediab_b, ..
            },
        ) => median_a.partial_cmp(mediab_b).unwrap(),
        (LatencyResult::Success { .. }, _) => std::cmp::Ordering::Less,
        (_, LatencyResult::Success { .. }) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });

    // Display results
    println!("{}", "=".repeat(110));
    println!(
        "{:<4} {:<8} {:<8} {:<8} {:<8} {:<8} {:<45}",
        "排名", "端口", "med", "avg", "min", "max", "节点名称 (tag)",
    );
    println!("{}", "-".repeat(110));

    for (rank, (tag, port, latency)) in results.iter().enumerate() {
        let rank = rank + 1;
        match latency {
            LatencyResult::Success {
                median,
                average,
                maximum,
                minimum,
            } => {
                println!("{:<4} {:<12} {median:<8.2} {average:<8.2} {minimum:<8.2} {maximum:<8.2} {tag:<45}", rank, port);
            }
            _ => {
                println!("{:<4} {:<12} {latency:35} {:<45}", rank, port, tag);
            }
        }
    }
    println!("{}", "=".repeat(110));

    Ok(())
}
