// quill 镜像 app-shell Service Worker(Phase 7 砖2 W3)。
//
// 目的:弱网 / 手机后台回来时首屏快 —— 把静态 app shell(index.html + vendored
// xterm.js/css/fit)缓存进 **Cache Storage**(独立于 HTTP 缓存,不受 daemon 发的
// `Cache-Control: no-store` 影响),命中直接返;WebSocket 数据面 + 任何动态请求
// 一律走网络(不拦)。忠实镜像保底不变:SW 只加速静态壳,拦不到 WS(WebSocket
// 不走 fetch 事件),坏了 / 没装 / 不支持也完全等价今天(渐进增强)。
//
// 版本 key(`__QUILL_VERSION__`)= daemon 按【内嵌 app-shell 内容哈希】在 serve /sw.js
// 时注入(见 daemon.rs `ASSET_VERSION`)。任一资产改动 → 哈希变 → 注入行变 → sw.js
// 字节变 → 浏览器检测到新 SW → install/activate 时清掉旧版本缓存 → 重建后旧 UI 不卡。
// 独立跑(没注入)兜底 "dev",不同名缓存互不干扰。
//
// ⚠️ Service Worker 需 **secure context**(https 或 localhost)。经 http://<VPN IP> 访问
// 时 `navigator.serviceWorker` 不可用 → 注册静默跳过、完全走网络(见 index.html 注册处)。
// 加 TLS(T7)后自动生效;此文件本身正确、面向未来。

"use strict";

// eslint-disable-next-line no-undef
const VERSION = (typeof self !== "undefined" && self.__QUILL_VERSION__) || "dev";
const CACHE_PREFIX = "quill-shell-";
const CACHE = CACHE_PREFIX + VERSION;

// 缓存的 app shell 精确路径集合(cache-first)。其余(WS / 未知 / 跨源)全走网络。
const SHELL = [
  "/",
  "/index.html",
  "/vendor/xterm.js",
  "/vendor/xterm.css",
  "/vendor/xterm-addon-fit.js",
];
const SHELL_SET = new Set(SHELL);

// install:预取 app shell 进本版本缓存。addAll 失败(离线首装 / 某资产 404)不让 install
// 硬失败 —— fetch 回退会按需回填(渐进增强,首屏永不因 SW 崩)。skipWaiting 让新版本尽快接管。
self.addEventListener("install", (event) => {
  self.skipWaiting();
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .catch(() => {})
  );
});

// activate:删掉所有【别的版本】的 shell 缓存(重建后旧缓存作废),再 claim 现有页面。
// 只删本 SW 自己的前缀命名空间,不碰其它来源可能建的缓存。
self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const keys = await caches.keys();
      await Promise.all(
        keys
          .filter((k) => k.startsWith(CACHE_PREFIX) && k !== CACHE)
          .map((k) => caches.delete(k))
      );
      await self.clients.claim();
    })()
  );
});

// fetch:只拦【同源 GET 的 app shell】走 cache-first;非 GET / 跨源 / 非 shell(动态、
// WS upgrade 等)一律不调 respondWith → 浏览器默认走网络。WebSocket 连接根本不触发
// fetch 事件,故数据面天然不受影响。
self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  let url;
  try {
    url = new URL(req.url);
  } catch (_) {
    return;
  }
  if (url.origin !== self.location.origin) return;
  if (!SHELL_SET.has(url.pathname)) return;
  event.respondWith(cacheFirst(req));
});

// cache-first:命中即返(不受 no-store 影响);未命中(首屏 SW 刚装、addAll 曾失败)→
// 网络取并回填;离线且无缓存 → 对导航兜底到缓存的 index.html(有则给,无则抛让浏览器报错)。
async function cacheFirst(req) {
  const cache = await caches.open(CACHE);
  const hit = await cache.match(req);
  if (hit) return hit;
  try {
    const res = await fetch(req);
    if (res && res.ok) cache.put(req, res.clone());
    return res;
  } catch (err) {
    if (req.mode === "navigate") {
      const fallback = await cache.match("/index.html");
      if (fallback) return fallback;
    }
    throw err;
  }
}
