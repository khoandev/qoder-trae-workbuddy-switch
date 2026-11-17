/**
 * 管理面登录页（服务端 `BUDDY_SWITCH_AUTH_KEY`）。
 *
 * ## 为什么是独立页面而不是应用内弹窗
 *
 * 登录前**不该渲染任何应用数据**：弹窗方案要求应用壳已经挂载并开始取数，
 * 那些请求会先撞一串 401，页面上留下一堆报错和空白区块。整页接管（见
 * `auth-gate.tsx`）让「未登录」和「已登录」是两棵完全不同的树。
 *
 * ## 表单原语全部复用 shadcn
 *
 * `Card` / `Label` / `Input` / `Button` / `Alert` —— 焦点管理、`aria` 关联、
 * 主题 token 都走项目既有实现，不手搓 input 样式。
 */

import { useState, type FormEvent } from "react";
import { KeyRound, Loader2, ShieldCheck } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { asError } from "@/lib/api";
import { useT } from "@/lib/i18n";
import { loginWebui } from "@/lib/webui-auth";

export function LoginPage() {
  const t = useT();
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit = key.trim().length > 0 && !busy;

  async function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      // 成功后 store 切到 `authenticated`，`AuthGate` 会把整棵树换掉（本组件随之卸载），
      // 因此这里**不需要**再 setBusy(false)。
      await loginWebui(key.trim());
    } catch (e) {
      setError(asError(e));
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-background px-4 py-10">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <div className="flex size-9 items-center justify-center rounded-lg bg-muted text-muted-foreground">
            <ShieldCheck className="size-4.5" aria-hidden />
          </div>
          <CardTitle className="mt-2">{t("auth.login.title")}</CardTitle>
          <CardDescription>{t("auth.login.subtitle")}</CardDescription>
        </CardHeader>
        <CardContent>
          <form className="space-y-4" onSubmit={onSubmit}>
            <div className="space-y-2">
              <Label htmlFor="buddy-switch-auth-key">{t("auth.login.keyLabel")}</Label>
              <div className="relative">
                <KeyRound
                  className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground"
                  aria-hidden
                />
                <Input
                  id="buddy-switch-auth-key"
                  type="password"
                  autoFocus
                  autoComplete="current-password"
                  spellCheck={false}
                  className="pl-9"
                  placeholder={t("auth.login.keyPlaceholder")}
                  value={key}
                  onChange={(event) => setKey(event.target.value)}
                />
              </div>
            </div>
            {error && (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            )}
            <Button type="submit" className="w-full" disabled={!canSubmit} aria-busy={busy}>
              {busy ? (
                <>
                  <Loader2 className="size-4 animate-spin" aria-hidden />
                  {t("auth.login.submitting")}
                </>
              ) : (
                t("auth.login.submit")
              )}
            </Button>
          </form>
          <p className="mt-4 text-xs leading-5 text-muted-foreground">{t("auth.login.hint")}</p>
        </CardContent>
      </Card>
    </div>
  );
}
