//! HTTP transport: `http://` / `https://` (build feature `http-cache`).

#[cfg(feature = "http-cache")]
use anyhow::Context;
use anyhow::Result;

use super::Transport;

// ---- HttpTransport: http:// / https:// (feature `http-cache`) ---------------

/// One HTTP response reduced to what the transport needs: status + body. A
/// non-2xx status is data here (e.g. 404 → "no such object"), not an error;
/// only a connect/transport failure surfaces as `Err`.
#[cfg(feature = "http-cache")]
pub struct HttpResp {
    pub status: u16,
    pub body: Vec<u8>,
}

/// GET/PUT/DELETE over HTTP. Behind a trait so `HttpTransport` is unit-testable
/// without a live server; `ureq` lives only in `RealHttpClient`.
#[cfg(feature = "http-cache")]
pub trait HttpClient {
    fn get(&self, url: &str) -> Result<HttpResp>;
    fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp>;
    fn delete(&self, url: &str) -> Result<HttpResp>;
}

/// Shared-cache transport over a plain HTTP(S) endpoint. Reads/writes
/// `<root>/base.json` and `<root>/segments/seg-<id>.json`; **enumeration
/// requires the endpoint to answer `GET <root>/segments/` with a JSON array of
/// names** (filenames or full keys/URLs) — the HTTP listing contract, since
/// bare GET/PUT can't list a collection.
#[cfg(feature = "http-cache")]
pub struct HttpTransport {
    root: String,
    client: Box<dyn HttpClient>,
}

#[cfg(feature = "http-cache")]
impl HttpTransport {
    pub fn new(remote: &str, client: Box<dyn HttpClient>) -> Self {
        Self {
            root: remote.trim_end_matches('/').to_string(),
            client,
        }
    }
    fn base_url(&self) -> String {
        format!("{}/base.json", self.root)
    }
    fn segment_url(&self, id: &str) -> String {
        format!("{}/segments/seg-{id}.json", self.root)
    }
    fn segments_url(&self) -> String {
        format!("{}/segments/", self.root)
    }
}

#[cfg(feature = "http-cache")]
fn is_2xx(status: u16) -> bool {
    (200..300).contains(&status)
}

#[cfg(feature = "http-cache")]
impl Transport for HttpTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let url = self.segments_url();
        let r = self.client.get(&url)?;
        if r.status == 404 {
            return Ok(Vec::new());
        }
        if !is_2xx(r.status) {
            anyhow::bail!("listing {url}: HTTP {}", r.status);
        }
        let names: Vec<String> = serde_json::from_slice(&r.body).with_context(|| {
            format!(
                "listing {url}: expected a JSON array of segment names \
                 (the HTTP cache listing contract)"
            )
        })?;
        let mut ids = Vec::new();
        for name in names {
            let base = name.rsplit('/').next().unwrap_or(&name);
            if let Some(id) = base
                .strip_prefix("seg-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let url = self.segment_url(id);
        let r = self.client.get(&url)?;
        match r.status {
            404 => Ok(None),
            s if is_2xx(s) => Ok(Some(r.body)),
            s => anyhow::bail!("reading {url}: HTTP {s}"),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        let url = self.base_url();
        let r = self.client.get(&url)?;
        match r.status {
            404 => Ok(None),
            s if is_2xx(s) => Ok(Some(r.body)),
            s => anyhow::bail!("reading {url}: HTTP {s}"),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let url = self.segment_url(id);
        let r = self.client.put(&url, bytes)?;
        if is_2xx(r.status) {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: HTTP {}", r.status)
        }
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        let url = self.base_url();
        let r = self.client.put(&url, bytes)?;
        if is_2xx(r.status) {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: HTTP {}", r.status)
        }
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        let url = self.segment_url(id);
        let r = self.client.delete(&url)?;
        if is_2xx(r.status) || r.status == 404 {
            Ok(())
        } else {
            anyhow::bail!("deleting {url}: HTTP {}", r.status)
        }
    }
}

/// `ureq`-backed client. Bearer auth from `RSTEST_CACHE_REMOTE_TOKEN` (blank =
/// none). Blocking, no async runtime.
#[cfg(feature = "http-cache")]
struct RealHttpClient {
    agent: ureq::Agent,
    auth: Option<String>,
}

#[cfg(feature = "http-cache")]
impl RealHttpClient {
    fn from_env() -> Self {
        let auth = std::env::var("RSTEST_CACHE_REMOTE_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|t| format!("Bearer {t}"));
        // Non-2xx statuses come back as `Ok` so `exec` can fold them into
        // `HttpResp`; only transport failures are `Err`.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            auth,
        }
    }
    fn with_auth<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match &self.auth {
            Some(v) => req.header("Authorization", v),
            None => req,
        }
    }
    /// Read a response (any status) into `HttpResp`. The body reader is
    /// unbounded, matching ureq 2's `into_reader`: cache artifacts can exceed
    /// ureq's 10 MiB `read_to_vec` default.
    fn collect(outcome: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<HttpResp> {
        use std::io::Read;
        let resp = outcome.map_err(|e| anyhow::Error::new(e).context("HTTP request failed"))?;
        let status = resp.status().as_u16();
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .read_to_end(&mut buf)
            .context("reading HTTP response body")?;
        Ok(HttpResp { status, body: buf })
    }
}

#[cfg(feature = "http-cache")]
impl HttpClient for RealHttpClient {
    fn get(&self, url: &str) -> Result<HttpResp> {
        Self::collect(self.with_auth(self.agent.get(url)).call())
    }
    fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp> {
        Self::collect(self.with_auth(self.agent.put(url)).send(body))
    }
    fn delete(&self, url: &str) -> Result<HttpResp> {
        Self::collect(self.with_auth(self.agent.delete(url)).call())
    }
}

#[cfg(feature = "http-cache")]
pub(super) fn http_transport(remote: &str) -> Result<Box<dyn Transport>> {
    Ok(Box::new(HttpTransport::new(
        remote,
        Box::new(RealHttpClient::from_env()),
    )))
}

#[cfg(not(feature = "http-cache"))]
pub(super) fn http_transport(remote: &str) -> Result<Box<dyn Transport>> {
    anyhow::bail!(
        "cache remote {remote} needs the `http-cache` build feature, \
         which is not compiled into this binary"
    )
}

#[cfg(all(test, feature = "http-cache"))]
mod tests {
    use std::cell::RefCell;

    use super::super::transport_for;
    use super::*;

    // ---- HttpTransport (http:// / https://) --------------------------------

    type HttpResponder = Box<dyn Fn(&str, &str, Option<&[u8]>) -> HttpResp>;
    struct StubHttpClient {
        responder: HttpResponder,
        calls: RefCell<Vec<(String, String)>>,
    }
    impl StubHttpClient {
        fn new(responder: impl Fn(&str, &str, Option<&[u8]>) -> HttpResp + 'static) -> Box<Self> {
            Box::new(StubHttpClient {
                responder: Box::new(responder),
                calls: RefCell::new(Vec::new()),
            })
        }
    }
    impl HttpClient for StubHttpClient {
        fn get(&self, url: &str) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("GET".into(), url.into()));
            Ok((self.responder)("GET", url, None))
        }
        fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("PUT".into(), url.into()));
            Ok((self.responder)("PUT", url, Some(body)))
        }
        fn delete(&self, url: &str) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("DELETE".into(), url.into()));
            Ok((self.responder)("DELETE", url, None))
        }
    }
    fn resp(status: u16, body: &[u8]) -> HttpResp {
        HttpResp {
            status,
            body: body.to_vec(),
        }
    }

    #[test]
    fn http_list_parses_json_array_and_ignores_non_segments() {
        let t = HttpTransport::new(
            "https://cache.example/team/",
            StubHttpClient::new(|_m, url, _b| {
                assert_eq!(url, "https://cache.example/team/segments/");
                // Mix of bare filenames, a full URL, and a non-segment entry.
                resp(
                    200,
                    br#"["seg-run1-1of2.json","https://cache.example/team/segments/seg-run1-2of2.json","index.html"]"#,
                )
            }),
        );
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["run1-1of2".to_string(), "run1-2of2".to_string()]);
    }

    #[test]
    fn http_list_404_is_empty_non_json_errors() {
        let empty =
            HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert!(empty.list_segment_ids().unwrap().is_empty());
        let bad = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(200, b"<html>not json</html>")),
        );
        assert!(bad.list_segment_ids().is_err());
    }

    #[test]
    fn http_read_base_404_none_2xx_some_5xx_error() {
        let none = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert_eq!(none.read_base().unwrap(), None);
        let some = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_m, url, _| {
                assert_eq!(url, "https://c/x/base.json");
                resp(200, b"{}")
            }),
        );
        assert_eq!(some.read_base().unwrap(), Some(b"{}".to_vec()));
        let err = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(err.read_base().is_err());
    }

    #[test]
    fn http_write_puts_to_segment_url_and_surfaces_failure() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, body| {
                assert_eq!(m, "PUT");
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                assert_eq!(body, Some(b"payload".as_ref()));
                resp(201, b"")
            }),
        );
        ok.write_segment("a", b"payload").unwrap();
        let fail = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(403, b"denied")),
        );
        assert!(fail.write_segment("a", b"x").is_err());
    }

    #[test]
    fn http_delete_tolerates_404() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, _| {
                assert_eq!(m, "DELETE");
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                resp(404, b"")
            }),
        );
        assert!(t.delete_segment("a").is_ok());
    }

    #[test]
    fn http_transport_for_dispatches_and_no_junk_dir() {
        // http(s):// builds an HttpTransport (no network at construction).
        assert!(transport_for("https://cache/x").is_ok());
        assert!(transport_for("http://cache/x").is_ok());
    }

    #[test]
    fn http_real_client_reads_bearer_token_from_env() {
        // The env->Authorization mapping is pure; test it without a server.
        let held = crate::test_env::lock();
        let _env = crate::test_env::set_var(&held, "RSTEST_CACHE_REMOTE_TOKEN", "sekret");
        let c = RealHttpClient::from_env();
        assert_eq!(c.auth.as_deref(), Some("Bearer sekret"));
        let _env = crate::test_env::remove_var(&held, "RSTEST_CACHE_REMOTE_TOKEN");
        assert_eq!(RealHttpClient::from_env().auth, None);
    }

    // ---- HttpTransport: remaining status branches --------------------------

    #[test]
    fn http_list_non_2xx_surfaces() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(t.list_segment_ids().is_err());
    }

    #[test]
    fn http_read_segment_2xx_bytes_404_none_5xx_error() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_m, url, _| {
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                resp(200, b"blob")
            }),
        );
        assert_eq!(ok.read_segment("a").unwrap(), Some(b"blob".to_vec()));
        // 404 -> None (pruned mid-pull), a 5xx -> hard error.
        let miss = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert_eq!(miss.read_segment("a").unwrap(), None);
        let err = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(500, b"")));
        assert!(err.read_segment("a").is_err());
    }

    #[test]
    fn http_write_base_puts_and_surfaces_failure() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, _| {
                assert_eq!(m, "PUT");
                assert_eq!(url, "https://c/x/base.json");
                resp(200, b"")
            }),
        );
        ok.write_base(b"{}").unwrap();
        let fail = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(fail.write_base(b"{}").is_err());
    }

    #[test]
    fn http_delete_non_2xx_non_404_surfaces() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(t.delete_segment("a").is_err());
    }

    // ---- RealHttpClient over a localhost server ----------------------------

    #[test]
    fn real_http_client_roundtrips_over_localhost() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Serve 4 requests: GET 200, PUT 200, DELETE 200, GET 404.
        let handle = std::thread::spawn(move || {
            for (i, code) in [
                (0, "200 OK"),
                (1, "200 OK"),
                (2, "200 OK"),
                (3, "404 Not Found"),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                // Fully drain the request (headers + any Content-Length body)
                // before replying. On Windows, dropping the socket with
                // unconsumed inbound bytes triggers an abortive (RST) close,
                // which races the client's write and surfaces as a transport
                // error — the source of this test's flakiness.
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                let (head_end, content_len) = loop {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break (req.len(), 0usize);
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let text = String::from_utf8_lossy(&req[..pos]);
                        let cl = text
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        break (pos + 4, cl);
                    }
                };
                let mut remaining = content_len.saturating_sub(req.len() - head_end);
                while remaining > 0 {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    remaining = remaining.saturating_sub(n);
                }
                let body: &[u8] = if i == 3 { b"" } else { b"ok" };
                let head = format!(
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
                let _ = s.flush();
                // Graceful half-close: signal EOF, let the client finish
                // reading before the socket drops.
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
        });
        // Token set => with_auth attaches Authorization.
        let held = crate::test_env::lock();
        let token = crate::test_env::set_var(&held, "RSTEST_CACHE_REMOTE_TOKEN", "tok");
        let c = RealHttpClient::from_env();
        drop(token);
        let base = format!("http://{addr}");
        let g = c.get(&format!("{base}/base.json")).unwrap();
        assert_eq!(g.status, 200);
        assert_eq!(g.body, b"ok");
        let p = c.put(&format!("{base}/seg"), b"data").unwrap();
        assert_eq!(p.status, 200);
        let d = c.delete(&format!("{base}/seg")).unwrap();
        assert_eq!(d.status, 200);
        // A non-2xx status is folded into HttpResp, not an Err.
        let nf = c.get(&format!("{base}/missing")).unwrap();
        assert_eq!(nf.status, 404);
        handle.join().unwrap();
        // A connection failure (no auth => with_auth None branch) is an Err.
        let bad = RealHttpClient::from_env().get("http://127.0.0.1:1/x");
        assert!(bad.is_err());
    }
}
