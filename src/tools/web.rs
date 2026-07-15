use super::*;

const WEBFETCH_MAX_BYTES: usize = 5 * 1024 * 1024;
const WEBFETCH_DEFAULT_TIMEOUT: u64 = 30;
const WEBFETCH_MAX_TIMEOUT: u64 = 120;
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

fn webfetch_accept_header(format: &str) -> &'static str {
    match format {
        "markdown" => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1",
        _ => "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    }
}

fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet"
}

fn is_textual_mime(mime: &str) -> bool {
    mime.is_empty()
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "application/javascript"
        || mime == "application/x-javascript"
}

pub(super) async fn webfetch(args: &str) -> Result<String> {
    let args: WebFetchArgs = serde_json::from_str(args)?;
    let url = args.url.trim();
    if url.is_empty() {
        return Err(anyhow!("url is empty"));
    }
    let url = if url.starts_with("http://") {
        url.replace("http://", "https://")
    } else if !url.starts_with("https://") {
        return Err(anyhow!("URL must start with http:// or https://"));
    } else {
        url.to_string()
    };
    let format = args.format.as_deref().unwrap_or("markdown");
    if !["text", "markdown", "html"].contains(&format) {
        return Err(anyhow!("format must be text, markdown, or html"));
    }
    let timeout_secs = args
        .timeout
        .map(|t| t as u64)
        .unwrap_or(WEBFETCH_DEFAULT_TIMEOUT)
        .clamp(1, WEBFETCH_MAX_TIMEOUT);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    let do_request = |ua: &str| {
        client
            .get(&url)
            .header("User-Agent", ua)
            .header("Accept", webfetch_accept_header(format))
            .header("Accept-Language", "en-US,en;q=0.9")
    };

    let mut resp = do_request(BROWSER_UA).send().await?;
    // Cloudflare bot detection: retry once with an honest UA on a 403 challenge.
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        if let Some(v) = resp.headers().get("cf-mitigated") {
            if v.to_str().unwrap_or("") == "challenge" {
                resp = do_request("opencode").send().await?;
            }
        }
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}"));
    }

    // Content-length preflight (advisory; real guard is the streaming cap below).
    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(s) = cl.to_str() {
            if let Ok(n) = s.parse::<usize>() {
                if n > WEBFETCH_MAX_BYTES {
                    return Err(anyhow!(
                        "Response too large (content-length {n} exceeds {} byte limit)",
                        WEBFETCH_MAX_BYTES
                    ));
                }
            }
        }
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if is_image_mime(&mime) {
        return Err(anyhow!("Unsupported fetched image content type: {mime}"));
    }
    if !is_textual_mime(&mime) {
        return Err(anyhow!("Unsupported fetched file content type: {mime}"));
    }

    // Stream-cap the decompressed body so compression bombs and slow drips
    // cannot exhaust memory (reqwest auto-decompresses gzip/brotli/zstd/deflate).
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        if buf.len() > WEBFETCH_MAX_BYTES {
            return Err(anyhow!(
                "Response too large (exceeds {} byte limit)",
                WEBFETCH_MAX_BYTES
            ));
        }
    }
    let body = String::from_utf8_lossy(&buf).into_owned();

    let out = match format {
        "html" => body,
        "text" => html_to_text(&body),
        _ => html_to_markdown(&body),
    };
    Ok(format!("{} ({})\n{}", url, content_type, out))
}

fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 80)
}

fn html_to_markdown(html: &str) -> String {
    let text = html2text::from_read(html.as_bytes(), 80);
    if text == html || text.trim().is_empty() {
        return html.to_string();
    }
    text
}

const EXA_URL: &str = "https://mcp.exa.ai/mcp";
const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";
const WEBSEARCH_MAX_BYTES: usize = 256 * 1024;
const WEBSEARCH_NO_RESULTS: &str = "No search results found. Please try a different query.";

fn websearch_provider() -> &'static str {
    match std::env::var("OPENCODE_WEBSEARCH_PROVIDER")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("parallel") => "parallel",
        _ => "exa",
    }
}

/// Extracts the first `text` field from an MCP `tools/call` result envelope,
/// which may arrive as a single JSON object or as SSE `data:` lines.
fn parse_mcp_text(body: &str) -> Option<String> {
    let extract = |s: &str| -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        v.get("result")?
            .get("content")?
            .as_array()?
            .iter()
            .find_map(|c| c.get("text")?.as_str().map(|t| t.to_string()))
    };
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        if let Some(t) = extract(trimmed) {
            return Some(t);
        }
    }
    for line in body.lines() {
        if let Some(rest) = line.trim().strip_prefix("data: ") {
            if let Some(t) = extract(rest.trim()) {
                return Some(t);
            }
        }
    }
    None
}

pub(super) async fn websearch(args: &str) -> Result<String> {
    let args: WebSearchArgs = serde_json::from_str(args)?;
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return Err(anyhow!("query is empty"));
    }
    let year = chrono::Utc::now().year();
    let provider = websearch_provider();
    let num_results = args.num_results.unwrap_or(8).clamp(1, 20);
    let livecrawl = args.livecrawl.as_deref().unwrap_or("fallback");
    let search_type = args.search_type.as_deref().unwrap_or("auto");
    let context_max_chars = args.context_max_chars.unwrap_or(10000).clamp(1, 50000);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()?;

    let (url, tool_name, arguments, extra_headers): (
        &str,
        &str,
        serde_json::Value,
        Vec<(&str, String)>,
    ) = match provider {
        "parallel" => {
            let mut h: Vec<(&str, String)> = vec![("User-Agent", "opencode".to_string())];
            if let Ok(k) = std::env::var("PARALLEL_API_KEY") {
                h.push(("Authorization", format!("Bearer {k}")));
            }
            (
                PARALLEL_URL,
                "web_search",
                serde_json::json!({
                    "objective": query,
                    "search_queries": [query],
                }),
                h,
            )
        }
        _ => {
            let h: Vec<(&str, String)> = vec![];
            (
                EXA_URL,
                "web_search_exa",
                serde_json::json!({
                    "query": query,
                    "type": search_type,
                    "numResults": num_results,
                    "livecrawl": livecrawl,
                    "contextMaxCharacters": context_max_chars,
                }),
                h,
            )
        }
    };

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool_name, "arguments": arguments }
    });

    let mut req = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&body);
    for (k, v) in &extra_headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("search failed (HTTP {status}): {text}"));
    }

    // Stream-cap the response so an oversized result cannot exhaust memory.
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        if buf.len() > WEBSEARCH_MAX_BYTES {
            return Err(anyhow!(
                "search response too large (exceeds {} byte limit)",
                WEBSEARCH_MAX_BYTES
            ));
        }
    }
    let resp_body = String::from_utf8_lossy(&buf).into_owned();

    let text = match parse_mcp_text(&resp_body) {
        Some(t) => t,
        None => return Ok(WEBSEARCH_NO_RESULTS.to_string()),
    };
    if text.trim().is_empty() {
        return Ok(WEBSEARCH_NO_RESULTS.to_string());
    }
    Ok(format!(
        "Search results for \"{query}\" ({year}):\n\n{text}"
    ))
}

pub(super) async fn run_command(args: &str, cwd: &Path) -> Result<String> {
    let args: CommandArgs = serde_json::from_str(args)?;
    if args.command.trim().is_empty() {
        return Err(anyhow!("command is empty"));
    }

    let timeout_secs = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        run_shell_command(&args.command, cwd, timeout_secs),
    )
    .await
    .map_err(|_| anyhow!("command timed out after {timeout_secs}s"))??;

    Ok(truncate(output, COMMAND_OUTPUT_LIMIT))
}

pub(super) async fn run_python_command(args: &str, cwd: &Path) -> Result<String> {
    let args: PythonArgs = serde_json::from_str(args)?;
    if args.code.trim().is_empty() {
        return Err(anyhow!("python code is empty"));
    }
    let timeout_secs = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        run_python(&args.code, cwd, timeout_secs),
    )
    .await
    .map_err(|_| anyhow!("python command timed out after {timeout_secs}s"))??;
    Ok(json!({
        "kind": "python_command_result",
        "code": args.code,
        "output": truncate(output, COMMAND_OUTPUT_LIMIT),
    })
    .to_string())
}

async fn run_python(code: &str, cwd: &Path, timeout_secs: u64) -> Result<String> {
    #[cfg(windows)]
    {
        match run_shell("python", &["-X", "utf8", "-c", code], cwd, timeout_secs).await {
            Ok(output) => Ok(output),
            Err(_) => run_shell("py", &["-3", "-X", "utf8", "-c", code], cwd, timeout_secs).await,
        }
    }

    #[cfg(not(windows))]
    {
        match run_shell("python3", &["-c", code], cwd, timeout_secs).await {
            Ok(output) => Ok(output),
            Err(_) => run_shell("python", &["-c", code], cwd, timeout_secs).await,
        }
    }
}

async fn run_shell_command(command: &str, cwd: &Path, timeout_secs: u64) -> Result<String> {
    #[cfg(windows)]
    {
        let utf8_preamble = "\
$OutputEncoding = [Console]::OutputEncoding = \
[System.Text.UTF8Encoding]::UTF8; \
$env:PYTHONIOENCODING='utf-8'; ";
        let full_cmd = format!("{utf8_preamble}{command}");
        let shell = windows_shell_program();
        return run_shell(
            shell,
            &["-NoLogo", "-NoProfile", "-Command", &full_cmd],
            cwd,
            timeout_secs,
        )
        .await;
    }

    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        run_shell(&shell, &["-lc", command], cwd, timeout_secs).await
    }
}

pub fn shell_guidance() -> String {
    #[cfg(windows)]
    {
        format!(
            "OS is Windows; shell is {} (PowerShell syntax). Use PowerShell cmdlets: `Get-ChildItem` (not `ls`), `Get-Content` (not `cat`), `Select-String` (not `grep`). Use `Get-ChildItem -Force` or `dir` for hidden/all files. Use double quotes for paths with spaces. Avoid Unix flags like `-la`, `-rf`.",
            windows_shell_program()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "OS is Unix-like; shell is {}. Use standard Unix commands. Quote paths with spaces using single or double quotes. Use `&&` to chain commands that must succeed, `;` when failures are acceptable.",
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
        )
    }
}

pub fn shell_description() -> String {
    #[cfg(windows)]
    {
        windows_shell_program().to_string()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
    }
}

pub fn os_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS
    }
}

#[cfg(windows)]
pub(super) fn windows_shell_program() -> &'static str {
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| {
        if std::process::Command::new("pwsh")
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-Command")
            .arg("$PSVersionTable.PSVersion | Out-Null")
            .status()
            .is_ok()
        {
            "pwsh"
        } else {
            "powershell"
        }
    })
}

async fn run_shell(program: &str, args: &[&str], cwd: &Path, timeout_secs: u64) -> Result<String> {
    let started = Instant::now();
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = strip_ansi(&stdout);
    let stderr = strip_ansi(&stderr);
    Ok(format!(
        "exit_code: {}\nwall_secs: {:.2}\ntimeout_secs: {}\nstdout:\n{}\nstderr:\n{}",
        output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "terminated".to_string()),
        started.elapsed().as_secs_f64(),
        timeout_secs,
        stdout,
        stderr
    ))
}

pub(super) fn strip_ansi(s: &str) -> String {
    let bytes = strip_ansi_escapes::strip(s);
    String::from_utf8_lossy(&bytes).to_string()
}
