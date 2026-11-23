//! Browser launch shared by the suites in this directory (a directory module, so it is not a
//! test binary). Each launch gets a Chrome profile of its own under `dir`, so tests run in
//! parallel without fighting over Chrome's `SingletonLock`.

use std::path::Path;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

pub async fn launch(dir: &Path) -> (Browser, tokio::task::JoinHandle<()>) {
    let chromium = std::env::var("MARGIN_CHROMIUM").expect(
        "MARGIN_CHROMIUM must name a chromium binary; this suite refuses to run without it",
    );
    let config = BrowserConfig::builder()
        .chrome_executable(chromium)
        .no_sandbox()
        .user_data_dir(dir.join("chrome-profile"))
        .args([
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--hide-scrollbars",
        ])
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.unwrap();
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });
    (browser, pump)
}
