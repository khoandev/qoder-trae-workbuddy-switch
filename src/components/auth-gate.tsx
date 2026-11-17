/**
 * 登录闸门：整个应用（含悬浮窗窗口）都在它后面。
 *
 * - `unknown` —— 还在问服务端。渲染一个中性的加载态，**不渲染应用**：否则会先闪
 *   一下空壳、再被登录页顶掉，用户看到的是「界面坏了」。
 * - `required` —— 整页登录页接管（`login-page.tsx`）。
 * - `disabled` / `authenticated` —— 正常渲染。
 *
 * 桌面端与演示模式恒为 `disabled`（见 `lib/webui-auth.ts`），于是这个组件在那边
 * 只是一层空壳，不会改变任何既有行为。
 */

import { useEffect, type ReactNode } from "react";
import { Loader2 } from "lucide-react";

import { LoginPage } from "@/components/login-page";
import { refreshWebuiAuth, useWebuiAuth } from "@/lib/webui-auth";

export function AuthGate({ children }: { children: ReactNode }) {
  const status = useWebuiAuth();

  // 启动探测一次。**不依赖 status**（依赖了会在切到 `required` 后无限重问）。
  useEffect(() => {
    void refreshWebuiAuth();
  }, []);

  if (status === "unknown") {
    return (
      <div
        className="flex min-h-screen items-center justify-center bg-background"
        aria-busy="true"
        aria-label="loading"
      >
        <Loader2 className="size-5 animate-spin text-muted-foreground" aria-hidden />
      </div>
    );
  }

  if (status === "required") return <LoginPage />;

  return <>{children}</>;
}
