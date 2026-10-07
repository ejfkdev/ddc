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
    println!("                    (GET/POST /info, /classes, /class, /method,");
    println!("                    /findrefs, /strings, /hierarchy, /disasm,");
    println!("                    /manifest, /mainactivity; GET binds query");
    println!("                    params, POST also accepts a JSON body)");
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
    println!("  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \\");
    println!("      -d '{{\"input\":\"app.apk\",\"class\":\"com.example.Foo\"}}'");
    println!();
    println!("  # bearer auth + TLS + CORS, one process:");
    println!("  ddc serve --addr 0.0.0.0:8443 --bearer tok1,tok2 \\");
    println!("      --tls-cert cert.pem --tls-key key.pem --cors 'https://app.example.com'");
    println!("  curl -H 'Authorization: Bearer tok1' \\");
    println!("      'https://host:8443/findrefs?input=app.apk&kind=string&query=token'");
    println!("  curl -H 'Authorization: Bearer tok2' 'https://host:8443/openapi.json'");
    println!("  # (no header → HTTP 401 with a JSON error body)");
    println!();
    println!("  # MCP client (Claude Desktop / any MCP client):");
    println!("  #   url: http://127.0.0.1:8080/mcp");
    println!("  #   (with --bearer: same URL + 'Authorization: Bearer <tok>' header)");
}

fn print_serve_help_zh() {
    println!("ddc serve — 同一端口的 HTTP REST + OpenAPI + MCP");
    println!();
    println!("用法：ddc serve [旗标]");
    println!();
    println!("启动一个网络进程，同一地址上服务三个前端：");
    println!("  REST              10 个查询命令即路由");
    println!("                    （GET/POST /info、/classes、/class、/method、");
    println!("                    /findrefs、/strings、/hierarchy、/disasm、");
    println!("                    /manifest、/mainactivity；GET 绑 query 参数，");
    println!("                    POST 还接受 JSON body）");
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
    println!("  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \\");
    println!("      -d '{{\"input\":\"app.apk\",\"class\":\"com.example.Foo\"}}'");
    println!();
    println!("  # 凭据 + TLS + CORS，一个进程：");
    println!("  ddc serve --addr 0.0.0.0:8443 --bearer tok1,tok2 \\");
    println!("      --tls-cert cert.pem --tls-key key.pem --cors 'https://app.example.com'");
    println!("  curl -H 'Authorization: Bearer tok1' \\");
    println!("      'https://host:8443/findrefs?input=app.apk&kind=string&query=token'");
    println!("  curl -H 'Authorization: Bearer tok2' 'https://host:8443/openapi.json'");
    println!("  # （缺凭据头 → HTTP 401，JSON 错误体）");
    println!();
    println!("  # MCP 客户端（Claude Desktop / 任意 MCP 客户端）：");
    println!("  #   url: http://127.0.0.1:8080/mcp");
    println!("  #   （带 --bearer 时：同一 URL + 'Authorization: Bearer <令牌>' 头）");
}

fn print_http_help_en() {
    println!("ddc http — HTTP REST + OpenAPI only (no MCP endpoint)");
    println!();
    println!("Usage: ddc http [FLAGS]");
    println!();
    println!("The REST-only form of serve: the 10 query routes (GET+POST, GET");
    println!("binds query params, POST also accepts a JSON body) and");
    println!("GET /openapi.json on one port, /mcp NOT mounted (use serve");
    println!("when MCP clients need the endpoint, mcp when REST is not");
    println!("wanted at all).");
    println!();
    println!("Flags: same as serve — --addr HOST:PORT (default :8080),");
    println!("  --bearer TOK[,TOK], --timeout 45s, --cors ORIGIN[,ORIG],");
    println!("  --tls-cert F --tls-key F, --default k=v.");
    println!();
    println!("Examples:");
    println!("  ddc http --addr 127.0.0.1:8080");
    println!("  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'");
    println!("  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \\");
    println!("      -d '{{\"input\":\"app.apk\",\"class\":\"com.example.Foo\"}}'");
    println!("  curl '127.0.0.1:8080/openapi.json' | python3 -m json.tool | head");
    println!();
    println!("  # bearer auth + TLS:");
    println!("  ddc http --addr 0.0.0.0:8443 --bearer tok1 \\");
    println!("      --tls-cert cert.pem --tls-key key.pem");
    println!("  curl -H 'Authorization: Bearer tok1' \\");
    println!("      'https://host:8443/strings?input=app.apk&filter=token'");
}

fn print_http_help_zh() {
    println!("ddc http — 仅 HTTP REST + OpenAPI（无 MCP 端点）");
    println!();
    println!("用法：ddc http [旗标]");
    println!();
    println!("serve 的纯 REST 形态：一个端口上挂 10 条查询路由（GET+POST，GET");
    println!("绑 query 参数，POST 还接受 JSON body）与 GET /openapi.json；");
    println!("/mcp 不挂载（MCP 客户端要端点用 serve；完全不");
    println!("要 REST 用 mcp）。");
    println!();
    println!("旗标：与 serve 相同 —— --addr 主机:端口（默认 :8080）、");
    println!("  --bearer 令牌[,令牌]、--timeout 45s、--cors 来源[,来源]、");
    println!("  --tls-cert F --tls-key F、--default k=v。");
    println!();
    println!("示例：");
    println!("  ddc http --addr 127.0.0.1:8080");
    println!("  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'");
    println!("  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \\");
    println!("      -d '{{\"input\":\"app.apk\",\"class\":\"com.example.Foo\"}}'");
    println!("  curl '127.0.0.1:8080/openapi.json' | python3 -m json.tool | head");
    println!();
    println!("  # 凭据 + TLS：");
    println!("  ddc http --addr 0.0.0.0:8443 --bearer tok1 \\");
    println!("      --tls-cert cert.pem --tls-key key.pem");
    println!("  curl -H 'Authorization: Bearer tok1' \\");
    println!("      'https://host:8443/strings?input=app.apk&filter=token'");
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
    println!("  ddc mcp stdio                # agent config: command = ddc,");
    println!("                               #                args = ['mcp', 'stdio']");
    println!();
    println!("  # Claude Desktop — claude_desktop_config.json:");
    println!("  #   'mcpServers': {{ 'ddc': {{ 'command': 'ddc',");
    println!("  #                    'args': ['mcp', 'stdio'] }} }}");
    println!();
    println!("  # authenticated streamable HTTP + pinned spec revisions:");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --bearer tok1 \\");
    println!("      --versions 2025-06-18,2026-07-28");
    println!();
    println!("  # session timeout + server identity in the handshake:");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --session-timeout 30m \\");
    println!("      --name ddc-tools --server-version 0.1.25");
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
    println!("  ddc mcp stdio                # Agent 配置：command = ddc,");
    println!("                               #                args = ['mcp', 'stdio']");
    println!();
    println!("  # Claude Desktop（claude_desktop_config.json）：");
    println!("  #   'mcpServers': {{ 'ddc': {{ 'command': 'ddc',");
    println!("  #                    'args': ['mcp', 'stdio'] }} }}");
    println!();
    println!("  # 带凭据的流式 HTTP + 锁定协议修订：");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --bearer tok1 \\");
    println!("      --versions 2025-06-18,2026-07-28");
    println!();
    println!("  # 会话超时 + 握手中的服务器身份：");
    println!("  ddc mcp http --addr 127.0.0.1:9300 --session-timeout 30m \\");
    println!("      --name ddc-tools --server-version 0.1.25");
}

/// `ddc help serve` / `ddc help mcp` / `ddc serve -h` / `ddc mcp -h`./// `ddc help serve` / `ddc help mcp` / `ddc serve -h` / `ddc mcp -h`.
pub(crate) fn print_mode_help(mode: &str) {
    match mode {
        "serve" => match crate::lang::lang() {
            crate::lang::Lang::Zh => print_serve_help_zh(),
            crate::lang::Lang::En => print_serve_help_en(),
        },
        "http" => match crate::lang::lang() {
            crate::lang::Lang::Zh => print_http_help_zh(),
            crate::lang::Lang::En => print_http_help_en(),
        },
        "mcp" => match crate::lang::lang() {
            crate::lang::Lang::Zh => print_mcp_help_zh(),
            crate::lang::Lang::En => print_mcp_help_en(),
        },
        _ => {}
    }}
