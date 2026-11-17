/**
 * 管理面**登录态**（内置登录，服务端 `BUDDY_SWITCH_AUTH_KEY`）。
 *
 * ## 为什么是一个模块级 store
 *
 * 登录态有**两个消费者**：`AuthGate`（决定渲染登录页还是应用）与设置页的
 * 「退出登录」块（只在已登录时出现）。再加上「任意接口 401 ⇒ 立刻回到登录页」这条
 * 全局反应 —— 各自 `useState` 就会出现「一个说已登录、一个说未登录」的分叉。故收敛成
 * 模块级 store + `useSyncExternalStore`（与 `lib/shortcut-preference.ts` /
 * `lib/i18n.ts` 同构）。
 *
 * ## 三条状态语义
 *
 * - `unknown` —— 还没问过服务端（启动瞬间）。**不渲染应用**：否则会先闪一下空壳。
 * - `disabled` —— 服务端没开登录（或桌面端 / 演示模式，压根没有这个面）。
 * - `required` —— 服务端开了登录，但当前没有有效会话。
 * - `authenticated` —— 有有效会话。
 *
 * ## 密钥不落浏览器存储
 *
 * 密钥只出现在 `POST /api/auth/login` 的**请求体**里，之后服务端下发 `HttpOnly` Cookie。
 * 这里**不**写 `localStorage` / `sessionStorage`：那等于把口令交给任何一段 XSS。
 */

import { useSyncExternalStore } from "react";

import { AUTH_REQUIRED_EVENT, apiBaseUrl, isDemoMode, isWebui } from "./api";
import { t } from "./i18n";

export type WebuiAuthStatus = "unknown" | "disabled" | "required" | "authenticated";

/**
 * 初始状态。桌面端 / 演示模式直接 `disabled`：它们没有 HTTP 管理面，
 * 也就不存在「登录」这件事（Tauri 走 `invoke`）。
 */
const isAuthSurface = typeof window !== "undefined" && isWebui() && !isDemoMode();

let status: WebuiAuthStatus = isAuthSurface ? "unknown" : "disabled";

const listeners = new Set<() => void>();

function setStatus(next: WebuiAuthStatus): void {
  if (next === status) return;
  status = next;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function getSnapshot(): WebuiAuthStatus {
  return status;
}

/** 订阅登录态（React 组件用）。 */
export function useWebuiAuth(): WebuiAuthStatus {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** 服务端 `/api/auth/status` 的响应形状（只取用到的两个字段）。 */
type AuthStatusBody = { required?: boolean; authenticated?: boolean };

/**
 * 问一次服务端「要不要登录 / 现在登录了没」，并按结果更新状态。
 *
 * 网络 / 解析失败时落到 `required`：让登录页显示出来并由提交时的错误提示去解释
 * 「连不上服务」。反过来（失败即当作已登录）会让登录页永远不出现。
 */
export async function refreshWebuiAuth(): Promise<void> {
  if (!isAuthSurface || status === "disabled") return;
  let body: AuthStatusBody;
  try {
    const response = await fetch(`${apiBaseUrl()}/api/auth/status`, {
      credentials: "same-origin",
      cache: "no-store",
    });
    if (!response.ok) throw new Error(String(response.status));
    body = (await response.json()) as AuthStatusBody;
  } catch {
    setStatus("required");
    return;
  }
  if (body.required === false) setStatus("disabled");
  else if (body.required && body.authenticated) setStatus("authenticated");
  else setStatus("required");
}

/**
 * 用密钥登录。成功 ⇒ `authenticated`；失败抛**已本地化**的错误（登录页直接显示）。
 */
export async function loginWebui(key: string): Promise<void> {
  let response: Response;
  try {
    response = await fetch(`${apiBaseUrl()}/api/auth/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify({ key }),
    });
  } catch {
    throw new Error(t("shared.api.unreachable", { base: apiBaseUrl() }));
  }
  const data = (await response.json().catch(() => ({}))) as { error?: string };
  if (response.status === 401) throw new Error(t("auth.login.invalid"));
  if (!response.ok) throw new Error(data.error || t("auth.login.failed"));
  setStatus("authenticated");
}

/**
 * 退出登录：让服务端吊销会话并清 Cookie，然后把界面切回登录页。
 *
 * 失败时**不改状态** —— 服务端还认这个会话，界面装作已登出只会误导用户。
 */
export async function logoutWebui(): Promise<void> {
  let response: Response;
  try {
    response = await fetch(`${apiBaseUrl()}/api/auth/logout`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "same-origin",
      body: "{}",
    });
  } catch {
    throw new Error(t("auth.logout.failed"));
  }
  if (!response.ok) throw new Error(t("auth.logout.failed"));
  setStatus("required");
}

// 任意接口撞上 401（会话过期 / 另一个标签页登出）⇒ 立刻回登录页。
// 只在真的有管理面时注册：桌面端 / 演示模式的事件无从发生，也就不留监听器。
if (isAuthSurface) {
  window.addEventListener(AUTH_REQUIRED_EVENT, () => setStatus("required"));
}
