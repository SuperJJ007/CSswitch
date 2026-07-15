use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE};
use reqwest::redirect::Policy;
use serde_json::{json, Value};

pub const ROUTE_SKILL_NAME: &str = "csswitch-external-skill-tools";
const DISABLED_SKILL_NAME: &str = "customize";
const CONNECTOR_SERVER_ID: &str = "local:csswitch-skill-installer";
const OBSOLETE_CONNECTOR_SERVER_ID: &str = "local:csswitch-skill-uninstaller";
const CUSTOM_PROMPT_BEGIN: &str = "[CSSWITCH_EXTERNAL_SKILL_ROUTING_V1_BEGIN]";
const CUSTOM_PROMPT_END: &str = "[CSSWITCH_EXTERNAL_SKILL_ROUTING_V1_END]";
const CUSTOM_PROMPT_TEXT: &str = "For requests to install, import, or add an external Skill, first load and follow the attached `csswitch-external-skill-tools` Skill, then call the CSSwitch MCP `install_external_skill` tool with the user's exact public GitHub repository, collection, or Skill URL. The Agent must not download files, use shell or Python filesystem APIs, use GitHub credentials, call catalog or Skill Manager APIs, or call `host.agents.attach_skill`; CSSwitch performs archive download, validation, commit, and OPERON attachment. After `INSTALLED_ATTACHED_VERIFY_REQUIRED`, call `skill(skill_name)` and do not report the single Skill usable until it loads. After `BUNDLE_INSTALLED_ATTACHED`, report the returned Skill count without invoking every member and without claiming hooks or MCP were installed. Retry `FILES_COMMITTED_ATTACH_REQUIRED` or `ATTACH_STATE_UNCERTAIN` through the same install tool. For uninstall/remove/delete, use `uninstall_external_skill` exactly as the routing Skill instructs; a bundle member uninstalls its whole bundle. Never use `customize`, `host.skills.*`, `save_artifacts`, or manual file copying/deletion for these requests.";
const CUSTOM_PROMPT_MAX_BYTES: usize = 64 * 1024;
const CONTROL_URL_ENV: &str = "CSSWITCH_SCIENCE_CONTROL_URL";

pub fn run_cli(args: &[String]) -> Result<Value, String> {
    if args != ["configure-third-party"] {
        return Err("用法：science-control configure-third-party".into());
    }
    let url = std::env::var(CONTROL_URL_ENV).map_err(|_| "缺少本地 Science control URL")?;
    configure_third_party(&url)
}

fn configure_third_party(raw_url: &str) -> Result<Value, String> {
    let (origin, nonce) = validate_control_url(raw_url)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "初始化本地 Science 控制客户端失败")?;

    let auth = client
        .post(format!("{origin}/api/auth/nonce"))
        .header(ORIGIN, &origin)
        .form(&[("nonce", nonce.as_str()), ("dest", "/")])
        .send()
        .map_err(|_| "本地 Science nonce 认证请求失败")?;
    ensure_success(&auth, "nonce 认证")?;
    let auth_cookie =
        response_cookie(&auth, "operon_auth").ok_or("本地 Science nonce 认证未返回会话 cookie")?;

    let csrf_response = client
        .get(format!("{origin}/api/csrf"))
        .header(ORIGIN, &origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| "本地 Science CSRF 请求失败")?;
    ensure_success(&csrf_response, "CSRF 初始化")?;
    let csrf_cookie = response_cookie(&csrf_response, "operon_csrf")
        .ok_or("本地 Science CSRF 初始化未返回 cookie")?;

    let body = serde_json::to_vec(&json!({"skill_name": ROUTE_SKILL_NAME}))
        .map_err(|_| "编码路由 Skill 绑定请求失败")?;
    let attach = client
        .post(format!("{origin}/api/agents/OPERON/skills"))
        .header(ORIGIN, &origin)
        .header(
            COOKIE,
            format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
        )
        .header("x-operon-csrf", &csrf_cookie)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(|_| "绑定 CSSwitch 路由 Skill 的本地请求失败")?;
    ensure_success(&attach, "路由 Skill 绑定")?;

    let body = serde_json::to_vec(&json!({"server_id": CONNECTOR_SERVER_ID}))
        .map_err(|_| "编码 CSSwitch connector 绑定请求失败")?;
    let attach = client
        .post(format!("{origin}/api/agents/OPERON/connectors"))
        .header(ORIGIN, &origin)
        .header(
            COOKIE,
            format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
        )
        .header("x-operon-csrf", &csrf_cookie)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(|_| "绑定 CSSwitch connector 的本地请求失败")?;
    ensure_success(&attach, "CSSwitch connector 绑定")?;

    let mut connector_ids = get_agent_connector_ids(&client, &origin, &auth_cookie)?;
    if connector_ids
        .iter()
        .any(|id| id == OBSOLETE_CONNECTOR_SERVER_ID)
    {
        let detach = client
            .delete(format!(
                "{origin}/api/agents/OPERON/connectors/{OBSOLETE_CONNECTOR_SERVER_ID}"
            ))
            .header(ORIGIN, &origin)
            .header(
                COOKIE,
                format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
            )
            .header("x-operon-csrf", &csrf_cookie)
            .send()
            .map_err(|_| "清理旧 CSSwitch uninstaller connector 失败")?;
        ensure_success(&detach, "旧 CSSwitch uninstaller connector 清理")?;
        connector_ids = get_agent_connector_ids(&client, &origin, &auth_cookie)?;
    }
    if !connector_ids.iter().any(|id| id == CONNECTOR_SERVER_ID) {
        return Err("OPERON 未绑定 CSSwitch connector".into());
    }
    if connector_ids
        .iter()
        .any(|id| id == OBSOLETE_CONNECTOR_SERVER_ID)
    {
        return Err("OPERON 仍绑定旧 CSSwitch uninstaller connector".into());
    }

    let detach = client
        .delete(format!(
            "{origin}/api/agents/OPERON/skills/{DISABLED_SKILL_NAME}"
        ))
        .header(ORIGIN, &origin)
        .header(
            COOKIE,
            format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
        )
        .header("x-operon-csrf", &csrf_cookie)
        .send()
        .map_err(|_| "禁用 Science 官方远程 Skill 管理入口失败")?;
    ensure_success(&detach, "禁用 Science 官方远程 Skill 管理入口")?;

    let current_prompt = get_custom_prompt(&client, &origin, &auth_cookie)?;
    let managed_prompt = upsert_managed_prompt(&current_prompt)?;
    if managed_prompt != current_prompt {
        let body = serde_json::to_vec(&json!({"prompt_text": &managed_prompt}))
            .map_err(|_| "编码 CSSwitch 路由指令失败")?;
        let update = client
            .put(format!("{origin}/api/agents/OPERON/custom-prompt"))
            .header(ORIGIN, &origin)
            .header(
                COOKIE,
                format!("operon_auth={auth_cookie}; operon_csrf={csrf_cookie}"),
            )
            .header("x-operon-csrf", &csrf_cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| "写入 CSSwitch 路由指令的本地请求失败")?;
        ensure_success(&update, "CSSwitch 路由指令写入")?;
    }
    let verified_prompt = get_custom_prompt(&client, &origin, &auth_cookie)?;
    if verified_prompt != managed_prompt {
        return Err("Science 未保存完整的 CSSwitch 路由指令".into());
    }

    Ok(json!({
        "status": "CONFIGURED",
        "agent_name": "OPERON",
        "skill_name": ROUTE_SKILL_NAME,
        "connector_ids": [CONNECTOR_SERVER_ID],
        "disabled_skill": DISABLED_SKILL_NAME,
        "custom_prompt_managed": true
    }))
}

fn get_agent_connector_ids(
    client: &Client,
    origin: &str,
    auth_cookie: &str,
) -> Result<Vec<String>, String> {
    let response = client
        .get(format!(
            "{origin}/api/agents/OPERON/mcp-servers?include_tools=false"
        ))
        .header(ORIGIN, origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| "读取 OPERON connector 状态失败")?;
    ensure_success(&response, "OPERON connector 回读")?;
    let body = response
        .bytes()
        .map_err(|_| "读取 OPERON connector 回读响应失败")?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| "OPERON connector 回读响应非法")?;
    let items = value.as_array().ok_or("OPERON connector 回读缺少列表")?;
    Ok(items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

fn get_custom_prompt(client: &Client, origin: &str, auth_cookie: &str) -> Result<String, String> {
    let response = client
        .get(format!("{origin}/api/agents/OPERON/custom-prompt"))
        .header(ORIGIN, origin)
        .header(COOKIE, format!("operon_auth={auth_cookie}"))
        .send()
        .map_err(|_| "读取 OPERON 自定义指令失败")?;
    ensure_success(&response, "OPERON 自定义指令回读")?;
    let body = response
        .bytes()
        .map_err(|_| "读取 OPERON 自定义指令响应失败")?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| "OPERON 自定义指令响应非法")?;
    let prompt = match &value {
        Value::Null => "",
        Value::String(prompt) => prompt,
        Value::Object(object) => match object.get("prompt_text") {
            None | Some(Value::Null) => "",
            Some(Value::String(prompt)) => prompt,
            _ => return Err("OPERON 自定义指令响应字段非法".into()),
        },
        _ => return Err("OPERON 自定义指令响应格式不支持".into()),
    };
    if prompt.len() > CUSTOM_PROMPT_MAX_BYTES {
        return Err("OPERON 自定义指令过长，CSSwitch 未修改".into());
    }
    Ok(prompt.to_owned())
}

fn managed_prompt_block() -> String {
    format!("{CUSTOM_PROMPT_BEGIN}\n{CUSTOM_PROMPT_TEXT}\n{CUSTOM_PROMPT_END}")
}

fn upsert_managed_prompt(current: &str) -> Result<String, String> {
    let begin_count = current.matches(CUSTOM_PROMPT_BEGIN).count();
    let end_count = current.matches(CUSTOM_PROMPT_END).count();
    if begin_count > 1 || end_count > 1 || begin_count != end_count {
        return Err("CSSwitch 路由指令标记异常，未修改用户自定义指令".into());
    }

    let block = managed_prompt_block();
    if begin_count == 1 {
        let begin = current
            .find(CUSTOM_PROMPT_BEGIN)
            .ok_or("CSSwitch 路由指令缺少开始标记")?;
        let end_start = current
            .find(CUSTOM_PROMPT_END)
            .ok_or("CSSwitch 路由指令缺少结束标记")?;
        if end_start < begin {
            return Err("CSSwitch 路由指令标记顺序异常，未修改用户自定义指令".into());
        }
        let end = end_start + CUSTOM_PROMPT_END.len();
        let mut updated = String::with_capacity(current.len() + block.len());
        updated.push_str(&current[..begin]);
        updated.push_str(&block);
        updated.push_str(&current[end..]);
        if updated.len() > CUSTOM_PROMPT_MAX_BYTES {
            return Err("更新 CSSwitch 路由指令后长度超限，未修改用户自定义指令".into());
        }
        return Ok(updated);
    }

    if current.is_empty() {
        return Ok(block);
    }
    let separator = if current.ends_with("\n\n") {
        ""
    } else if current.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let updated = format!("{current}{separator}{block}");
    if updated.len() > CUSTOM_PROMPT_MAX_BYTES {
        return Err("添加 CSSwitch 路由指令后长度超限，未修改用户自定义指令".into());
    }
    Ok(updated)
}

fn validate_control_url(raw_url: &str) -> Result<(String, String), String> {
    let url = reqwest::Url::parse(raw_url).map_err(|_| "本地 Science control URL 非法")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("只允许带显式端口的本机 HTTP Science control URL".into());
    }
    let nonces: Vec<String> = url
        .query_pairs()
        .filter(|(name, _)| name == "nonce")
        .map(|(_, value)| value.into_owned())
        .collect();
    if nonces.len() != 1
        || nonces[0].is_empty()
        || nonces[0].len() > 512
        || !nonces[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
    {
        return Err("本地 Science control URL 缺少有效 nonce".into());
    }
    let host = url.host_str().expect("validated host");
    let port = url.port().expect("validated port");
    Ok((format!("http://{host}:{port}"), nonces[0].clone()))
}

fn response_cookie(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .filter_map(|header| header.split(';').next())
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(cookie_name, value)| {
            (cookie_name.trim() == name
                && !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && byte != b';'))
            .then(|| value.to_string())
        })
}

fn ensure_success(response: &Response, stage: &str) -> Result<(), String> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "本地 Science {stage}失败（HTTP {}）",
            response.status().as_u16()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while bytes.len() - header_end < length {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(bytes[..header_end + length].to_vec()).unwrap()
    }

    fn reply(stream: &mut TcpStream, cookie: Option<&str>, body: &str) {
        let cookie = cookie
            .map(|value| format!("Set-Cookie: {value}; Path=/; SameSite=Strict\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 200 OK\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    #[test]
    fn configures_third_party_skill_boundary_via_nonce_and_csrf_flow() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let existing_prompt = "Keep my existing preference.";
        let expected_prompt = upsert_managed_prompt(existing_prompt).unwrap();
        let expected_for_worker = expected_prompt.clone();
        let worker = thread::spawn(move || {
            let replies = vec![
                (Some("operon_auth=auth-token"), "{}".to_string()),
                (Some("operon_csrf=csrf-token"), "{}".to_string()),
                (None, "{}".to_string()),
                (None, "{}".to_string()),
                (
                    None,
                    r#"[{"id":"local:csswitch-skill-installer"},{"id":"local:csswitch-skill-uninstaller"}]"#.to_string(),
                ),
                (None, "{}".to_string()),
                (
                    None,
                    r#"[{"id":"local:csswitch-skill-installer"}]"#.to_string(),
                ),
                (None, "{}".to_string()),
                (None, serde_json::to_string(existing_prompt).unwrap()),
                (None, "{}".to_string()),
                (None, serde_json::to_string(&expected_for_worker).unwrap()),
            ];
            for (cookie, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                captured.lock().unwrap().push(read_request(&mut stream));
                reply(&mut stream, cookie, &body);
            }
        });
        let result =
            configure_third_party(&format!("http://127.0.0.1:{port}/?nonce=test-nonce")).unwrap();
        worker.join().unwrap();
        assert_eq!(result["status"], "CONFIGURED");
        assert_eq!(result["disabled_skill"], "customize");
        assert_eq!(result["custom_prompt_managed"], true);
        assert_eq!(
            result["connector_ids"],
            json!(["local:csswitch-skill-installer"])
        );
        assert!(result.get("denied_mutations").is_none());
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with("POST /api/auth/nonce HTTP/1.1"));
        assert!(requests[0].contains("nonce=test-nonce&dest=%2F"));
        assert!(requests[1].starts_with("GET /api/csrf HTTP/1.1"));
        assert!(requests[1].contains("operon_auth=auth-token"));
        assert!(requests[2].starts_with("POST /api/agents/OPERON/skills HTTP/1.1"));
        assert!(requests[2].contains("x-operon-csrf: csrf-token"));
        assert!(requests[2].contains("operon_auth=auth-token; operon_csrf=csrf-token"));
        assert!(requests[2].contains(&format!(r#"{{"skill_name":"{ROUTE_SKILL_NAME}"}}"#)));
        assert!(requests[3].starts_with("POST /api/agents/OPERON/connectors HTTP/1.1"));
        assert!(requests[3].contains("x-operon-csrf: csrf-token"));
        assert!(requests[3].contains(&format!(r#""server_id":"{CONNECTOR_SERVER_ID}""#)));
        assert!(requests[4]
            .starts_with("GET /api/agents/OPERON/mcp-servers?include_tools=false HTTP/1.1"));
        assert!(requests[5].starts_with(
            "DELETE /api/agents/OPERON/connectors/local:csswitch-skill-uninstaller HTTP/1.1"
        ));
        assert!(requests[5].contains("x-operon-csrf: csrf-token"));
        assert!(requests[6]
            .starts_with("GET /api/agents/OPERON/mcp-servers?include_tools=false HTTP/1.1"));
        assert!(requests[7].starts_with("DELETE /api/agents/OPERON/skills/customize HTTP/1.1"));
        assert!(requests[7].contains("x-operon-csrf: csrf-token"));
        assert!(requests[8].starts_with("GET /api/agents/OPERON/custom-prompt HTTP/1.1"));
        assert!(requests[9].starts_with("PUT /api/agents/OPERON/custom-prompt HTTP/1.1"));
        assert!(requests[9].contains("x-operon-csrf: csrf-token"));
        assert!(requests[9].contains(existing_prompt));
        assert!(requests[9].contains(CUSTOM_PROMPT_BEGIN));
        assert!(requests[9].contains(CUSTOM_PROMPT_END));
        assert!(requests[10].starts_with("GET /api/agents/OPERON/custom-prompt HTTP/1.1"));
        assert_eq!(
            upsert_managed_prompt(&expected_prompt).unwrap(),
            expected_prompt
        );
        assert!(requests
            .iter()
            .all(|request| !request.starts_with("PUT /api/approvals/grants HTTP/1.1")));
    }

    #[test]
    fn managed_prompt_preserves_user_text_and_replaces_only_its_marker_block() {
        let current = format!(
            "User prefix\n\n{CUSTOM_PROMPT_BEGIN}\nold route\n{CUSTOM_PROMPT_END}\n\nUser suffix"
        );
        let updated = upsert_managed_prompt(&current).unwrap();
        assert!(updated.starts_with("User prefix\n\n"));
        assert!(updated.ends_with("\n\nUser suffix"));
        assert!(updated.contains(CUSTOM_PROMPT_TEXT));
        assert!(!updated.contains("old route"));
        assert_eq!(upsert_managed_prompt(&updated).unwrap(), updated);
    }

    #[test]
    fn managed_prompt_rejects_malformed_or_duplicate_markers() {
        assert!(upsert_managed_prompt(CUSTOM_PROMPT_BEGIN).is_err());
        assert!(
            upsert_managed_prompt(&format!("{CUSTOM_PROMPT_END}\n{CUSTOM_PROMPT_BEGIN}")).is_err()
        );
        assert!(upsert_managed_prompt(&format!(
            "{CUSTOM_PROMPT_BEGIN}\n{CUSTOM_PROMPT_END}\n{CUSTOM_PROMPT_BEGIN}\n{CUSTOM_PROMPT_END}"
        ))
        .is_err());
    }

    #[test]
    fn rejects_non_loopback_or_missing_nonce_without_network_access() {
        assert!(validate_control_url("https://example.com:8990/?nonce=x").is_err());
        assert!(validate_control_url("http://127.0.0.1:8990/").is_err());
        assert!(validate_control_url("http://localhost/?nonce=x").is_err());
        assert!(validate_control_url("http://user@127.0.0.1:8990/?nonce=x").is_err());
    }
}
