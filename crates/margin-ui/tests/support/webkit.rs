//! A driver for Playwright's WebKit build, spoken to over its inspector pipe: JSON messages each
//! ended by a NUL byte, read by the browser on fd 3 and written by it on fd 4. It is Rust so the
//! repository stays one runtime. The page is a 390 by 844 touch phone; `tap` is WebKit's own
//! touch-tap input, which is the path that reaches the page as pointer, touch and click events.

// Included by every suite through `support`, most of which never open WebKit.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const SIGNAL_DEADLINE: Duration = Duration::from_secs(90);

pub struct WebKit {
    child: Child,
    to_browser: ChildStdin,
    from_browser: UnboundedReceiver<Value>,
    /// Messages that arrived while waiting for something else, oldest first.
    backlog: Vec<Value>,
    next_id: u64,
    page_proxy_id: Option<String>,
    target_id: Option<String>,
}

impl WebKit {
    /// Start the browser at `$MARGIN_WEBKIT`, open one phone-sized touch page and load `url` in it.
    pub async fn open(url: &str) -> WebKit {
        let binary = std::env::var("MARGIN_WEBKIT").expect(
            "MARGIN_WEBKIT must name Playwright's pw_run.sh for WebKit; this suite refuses to run without it",
        );
        // The pipes are the child's stdin and stdout; the shell moves them to fds 3 and 4 and
        // silences the real ones, so no unsafe code is needed to place them.
        let mut child = Command::new("sh")
            .args([
                "-c",
                "exec \"$0\" --inspector-pipe --headless --no-startup-window 3<&0 4>&1 </dev/null >/dev/null",
                &binary,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("starting {binary}: {e}"));
        let to_browser = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, from_browser) = unbounded_channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(0, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if buf.last() == Some(&0) {
                    buf.pop();
                }
                if let Ok(message) = serde_json::from_slice::<Value>(&buf)
                    && tx.send(message).is_err()
                {
                    return;
                }
            }
        });
        let mut wk = WebKit {
            child,
            to_browser,
            from_browser,
            backlog: Vec::new(),
            next_id: 0,
            page_proxy_id: None,
            target_id: None,
        };
        wk.browser("Playwright.enable", json!({}), false).await;
        let context = wk
            .browser("Playwright.createContext", json!({}), false)
            .await;
        let context_id = context["browserContextId"].clone();
        let page = wk
            .browser(
                "Playwright.createPage",
                json!({"browserContextId": context_id}),
                false,
            )
            .await;
        let proxy = page["pageProxyId"]
            .as_str()
            .expect("a pageProxyId")
            .to_string();
        wk.page_proxy_id = Some(proxy.clone());
        let created = wk
            .wait_message(|m| {
                m["method"] == "Target.targetCreated" && m["params"]["targetInfo"]["type"] == "page"
            })
            .await;
        wk.target_id = Some(
            created["params"]["targetInfo"]["targetId"]
                .as_str()
                .expect("a targetId")
                .to_string(),
        );
        wk.browser(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 390, "height": 844, "fixedLayout": true, "deviceScaleFactor": 1}),
            true,
        )
        .await;
        wk.page("Page.enable", json!({})).await;
        wk.page("Runtime.enable", json!({})).await;
        wk.page("Page.setTouchEmulationEnabled", json!({"enabled": true}))
            .await;
        wk.page(
            "Runtime.evaluate",
            json!({"expression": format!("location.href = {url:?}"), "returnByValue": true}),
        )
        .await;
        let want = format!("location.href === {url:?} && document.readyState === 'complete'");
        wk.wait_for(&format!("the page loaded {url}"), &want).await;
        wk
    }

    fn write(&mut self, message: &Value) {
        let mut bytes = serde_json::to_vec(message).unwrap();
        bytes.push(0);
        self.to_browser
            .write_all(&bytes)
            .and_then(|()| self.to_browser.flush())
            .expect("writing to the browser's inspector pipe");
    }

    async fn read(&mut self) -> Value {
        if !self.backlog.is_empty() {
            return self.backlog.remove(0);
        }
        match tokio::time::timeout(SIGNAL_DEADLINE, self.from_browser.recv()).await {
            Ok(Some(m)) => m,
            Ok(None) => panic!("the browser closed its inspector pipe"),
            Err(_) => panic!("the browser sent nothing for {SIGNAL_DEADLINE:?}"),
        }
    }

    /// The first message `want` accepts; others stay for later.
    async fn wait_message(&mut self, want: impl Fn(&Value) -> bool) -> Value {
        let mut skipped = Vec::new();
        let found = loop {
            let m = self.read().await;
            if want(&m) {
                break m;
            }
            skipped.push(m);
        };
        skipped.append(&mut self.backlog);
        self.backlog = skipped;
        found
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// A browser-level command; `on_page` adds the page proxy id to the message.
    async fn browser(&mut self, method: &str, params: Value, on_page: bool) -> Value {
        let id = self.id();
        let mut message = json!({"id": id, "method": method, "params": params});
        if on_page {
            message["pageProxyId"] = json!(self.page_proxy_id);
        }
        self.write(&message);
        let reply = self.wait_message(|m| m["id"] == id).await;
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    /// A command to the page itself: wrapped for the target, answered by its own message.
    async fn page(&mut self, method: &str, params: Value) -> Value {
        let inner = self.id();
        let wrapped = json!({"id": inner, "method": method, "params": params}).to_string();
        let target = self.target_id.clone();
        self.browser(
            "Target.sendMessageToTarget",
            json!({"targetId": target, "message": wrapped}),
            true,
        )
        .await;
        let mut found = None;
        while found.is_none() {
            let m = self
                .wait_message(|m| m["method"] == "Target.dispatchMessageFromTarget")
                .await;
            let text = m["params"]["message"].as_str().unwrap_or("{}");
            let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            if body["id"] == inner {
                found = Some(body);
            }
        }
        let reply = found.unwrap();
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    /// Evaluate `js` in the page and read its value.
    pub async fn eval<T: serde::de::DeserializeOwned>(&mut self, js: &str) -> T {
        let result = self
            .page(
                "Runtime.evaluate",
                json!({"expression": js, "returnByValue": true}),
            )
            .await;
        assert!(
            result["wasThrown"] != true,
            "evaluating {js} threw: {result}"
        );
        serde_json::from_value(result["result"]["value"].clone())
            .unwrap_or_else(|e| panic!("reading the value of {js}: {e}: {result}"))
    }

    /// Poll `js` until it is truthy: the page's own signal, with a deadline only to fail loudly.
    pub async fn wait_for(&mut self, what: &str, js: &str) {
        let started = Instant::now();
        loop {
            if self.eval::<bool>(&format!("Boolean({js})")).await {
                return;
            }
            assert!(
                started.elapsed() < SIGNAL_DEADLINE,
                "the page never signalled: {what} ({js})"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A touch tap at the viewport point (x, y).
    pub async fn tap(&mut self, x: f64, y: f64) {
        self.browser(
            "Input.dispatchTapEvent",
            json!({"x": x, "y": y, "modifiers": 0}),
            true,
        )
        .await;
    }

    /// Tap the centre of the first element matching `selector`.
    pub async fn tap_on(&mut self, selector: &str) {
        let (x, y): (f64, f64) = self
            .eval(&format!(
                "(() => {{ const r = document.querySelector({selector:?}).getBoundingClientRect();
                  return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ))
            .await;
        self.tap(x, y).await;
    }
}

impl Drop for WebKit {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
