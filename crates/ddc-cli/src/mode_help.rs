//! Help cards for the xyz mode words (`ddc serve`, `ddc mcp`) — the two
//! top-level words the xyz dispatcher owns. ddc's own help router and
//! the `-h` interception both print these; they also document what the
//! main help's HTTP & MCP section summarizes.

fn print_serve_help_en() {
    println!("ddc serve — HTTP REST + OpenAPI + MCP on one port");
    println!();
    println!("Usage: ddc serve [FLAGS]");
    println!();
    println!("Starts one network process serving three fronts on the same");
    println!("address:");
    println!("  REST              the 10 query commands as routes");
    println!("                    (GET /info, /classes, /class, /method, /findrefs,");
    println!("                    /strings, /hierarchy, /disasm, /manifest,");
    println!("                    /mainactivity)");
    println!("  GET /openapi.json the OpenAPI 3 document for those routes");
    println!("  GET /healthz      liveness probe");
    println!("  /mcp              streamable MCP endpoint — point an MCP client");
    println!("                    at http://<addr>/mcp (10 tools, input and");
    println!("                    output schemas, e.g. ddc.getclass)");
    println!();
    println!("Flags:");
    println!("  --addr HOST:PORT      listen address (default :8080)");
    println!("  --bearer TOK[,TOK]    require Authorization: Bearer <tok> on REST");
    println!("                        and /mcp (empty = no auth)");
    println!("  --timeout 45s         per-request timeout (0 = none)");
    println!("  --cors ORIGIN[,ORIG]  allowed CORS origins");
    println!("  --tls-cert F --tls-key F  switch to HTTPS (both required)");
    println!("  --default k=v         channel default filled into absent request");
    println!("                        keys (repeatable; unknown --key=value is");
    println!("                        shorthand for it)");
    println!();
    println!("Examples:");
    println!("  ddc serve --addr 127.0.0.1:8080");
    println!("  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'");
    println!("  # MCP client (Claude Desktop / any MCP client):");
    println!("  #   url: http://127.0.0.1:8080/mcp");
}

fn print_serve_help_zh() {
    println!("ddc serve — 同一端口的 HTTP REST + OpenAPI + MCP");
    println!();
    println!("用法：ddc serve [旗标]");
    println!();
    println!("启动一个网络进程，同一地址上服务三个前端：");
    println!("  REST              10 个查询命令即路由");
    println!("                    （GET /info、/classes、/class、/method、/findrefs、");
    println!("                    /strings、/hierarchy、/disasm、/manifest、");
    println!("                    /mainactivity）");
    println!("  GET /openapi.json 上述路由的 OpenAPI 3 文档");
    println!("  GET /healthz      存活探针");
    println!("  /mcp              流式 MCP 端点 —— 把 MCP 客户端指向");
    println!("                    http://<地址>/mcp（10 个工具，含输入/输出");
    println!("                    schema，如 ddc.getclass）");
    println!();
    println!("旗标：");
    println!("  --addr 主机:端口      监听地址（默认 :8080）");
    println!("  --bearer 令牌[,令牌]  REST 与 /mcp 要求 Authorization: Bearer");
    println!("                        <令牌>（留空 = 不鉴权）");
    println!("  --timeout 45s         每请求超时（0 = 无）");
    println!("  --cors 来源[,来源]    允许的 CORS 来源");
    println!("  --tls-cert F --tls-key F  切换 HTTPS（两者都要）");
    println!("  --default k=v         通道默认值，填入请求缺失的键（可重复；");
    println!("                        未识别的 --key=value 是它的简写）");
    println!();
    println!("示例：");
    println!("  ddc serve --addr 127.0.0.1:8080");
    println!("  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'");
    println!("  # MCP 客户端（Claude Desktop / 任意 MCP 客户端）：");
    println!("  #   url: http://127.0.0.1:8080/mcp");
}

fn print_mcp_help_en() {
    println!("ddc mcp — MCP tool server (no REST routes)");
    println!();
    println!("Usage: ddc mcp stdio|http [FLAGS]");
    println!();
    println!("  stdio   MCP over stdin/stdout — run as the agent's command, no");
    println!("          network (how Claude Desktop and local agents connect)");
    println!("  http    streamable-HTTP MCP server on its own address (SDK");
    println!("          default: loopback Host headers only; serve's /mcp is");
    println!("          the same protocol on the shared port)");
    println!();
    println!("Flags (http; stdio ignores address flags):");
    println!("  --addr HOST:PORT      listen address (default :8080)");
    println!("  --bearer TOK[,TOK]    required Authorization header");
    println!("  --versions A,B        pin MCP spec revisions (2024-11-05,");
    println!("                        2025-03-26, 2025-06-18, 2025-11-25,");
    println!("                        2026-07-28)");
    println!("  --stateless           streamable HTTP in stateless mode (the");
    println!("                        2026-07-28 handshake-free form, SEP-2567)");
    println!("  --json-response       prefer JSON responses where the spec allows");
    println!("  --session-timeout 30m session keep-alive window");
    println!("  --default k=v         channel default for absent tool keys");
    println!("  --name N              server name reported in the handshake");
    println!("  --server-version V    server version reported in the handshake");
    println!();
    println!("Every command becomes a tool: ddc.getclass, ddc.findrefs, ... —");
    println!("tools/list carries inputSchema and outputSchema.");
    println!();
    println!("Examples:");
    println!("  ddc mcp stdio                 # agent config: command = ddc, args = [mcp, stdio]");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --bearer tok1");
}

fn print_mcp_help_zh() {
    println!("ddc mcp — MCP 工具服务器（无 REST 路由）");
    println!();
    println!("用法：ddc mcp stdio|http [旗标]");
    println!();
    println!("  stdio   走 stdin/stdout 的 MCP —— 作为 Agent 的命令运行，不占");
    println!("          网络（Claude Desktop 与本地 Agent 的接入方式）");
    println!("  http    独立地址的流式 HTTP MCP 服务器（SDK 默认仅允许回环");
    println!("          Host；serve 的 /mcp 是同一协议共享端口形态）");
    println!();
    println!("旗标（http；stdio 忽略地址类旗标）：");
    println!("  --addr 主机:端口      监听地址（默认 :8080）");
    println!("  --bearer 令牌[,令牌]  要求的 Authorization 头");
    println!("  --versions A,B        锁定 MCP 协议修订（2024-11-05、");
    println!("                        2025-03-26、2025-06-18、2025-11-25、");
    println!("                        2026-07-28）");
    println!("  --stateless           流式 HTTP 无状态模式（2026-07-28 的免");
    println!("                        握手形态，SEP-2567）");
    println!("  --json-response       规范允许处优先 JSON 响应");
    println!("  --session-timeout 30m 会话保活窗口");
    println!("  --default k=v         工具缺失键的通道默认值");
    println!("  --name N              握手中上报的服务器名");
    println!("  --server-version V    握手中上报的服务器版本");
    println!();
    println!("每个命令成为一个工具：ddc.getclass、ddc.findrefs、… —— tools/list");
    println!("携带 inputSchema 与 outputSchema。");
    println!();
    println!("示例：");
    println!("  ddc mcp stdio                 # Agent 配置：command = ddc, args = [mcp, stdio]");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --bearer tok1");
}

/// `ddc help serve` / `ddc help mcp` / `ddc serve -h` / `ddc mcp -h`.
pub(crate) fn print_mode_help(mode: &str) {
    match mode {
        "serve" => match crate::lang::lang() {
            crate::lang::Lang::Zh => print_serve_help_zh(),
            crate::lang::Lang::En => print_serve_help_en(),
        },
        "mcp" => match crate::lang::lang() {
            crate::lang::Lang::Zh => print_mcp_help_zh(),
            crate::lang::Lang::En => print_mcp_help_en(),
        },
        _ => {}
    }}
