import { useEffect, useRef, useState } from "react";
import { Check, Copy } from "lucide-react";

import type { Service } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  cn,
  toDisplayUrl,
  isDnsOnly,
  isLocalHost,
  getRequestHostname,
} from "@/lib/utils";

/** Copy text to the clipboard, falling back to a hidden textarea (non-secure contexts). */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

/** Icon button that copies a URL and shows a checkmark for a moment. */
export function CopyUrlButton({ url }: { url: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    []
  );

  const onCopy = async (e: React.MouseEvent) => {
    e.stopPropagation();
    if (await copyText(url)) {
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1_500);
    }
  };

  return (
    <Button
      variant="ghost"
      size="icon-xs"
      type="button"
      onClick={onCopy}
      aria-label={copied ? "Copied" : `Copy ${url}`}
      title={copied ? "Copied" : "Copy URL"}
      className="text-muted-foreground hover:text-foreground shrink-0"
    >
      {copied ? <Check className="text-primary" /> : <Copy />}
    </Button>
  );
}

/**
 * Host-aware service URL with a copy button and a DNS-only badge. Clicks on the
 * link/button stop propagation so a parent row's "open logs" handler does not
 * also fire.
 */
export function ServiceUrl({
  svc,
  className,
  linkClassName,
}: {
  svc: Service;
  className?: string;
  linkClassName?: string;
}) {
  if (!svc.url) {
    return (
      <span className={cn("text-muted-foreground font-mono", className)}>
        —
      </span>
    );
  }
  const displayUrl = toDisplayUrl(svc.url, svc.ports);
  const dnsOnly =
    isDnsOnly(svc.url, svc.ports) && !isLocalHost(getRequestHostname());

  return (
    <div className={cn("flex min-w-0 items-center gap-1", className)}>
      <a
        href={displayUrl}
        target="_blank"
        rel="noreferrer"
        title={displayUrl}
        onClick={(e) => e.stopPropagation()}
        className={cn(
          "text-primary min-w-0 truncate font-mono underline-offset-4 hover:underline",
          linkClassName
        )}
      >
        {displayUrl}
      </a>
      <CopyUrlButton url={displayUrl} />
      {dnsOnly ? (
        <span
          title="Traefik-only, no host port published — reachable via DNS (*.gems/*.red-fox) or add ports: [8080] in compose"
          className="border-warning/30 bg-warning/10 text-warning shrink-0 rounded-full border px-1.5 py-0.5 font-mono text-[9px] tracking-wide uppercase"
        >
          DNS
        </span>
      ) : null}
    </div>
  );
}
