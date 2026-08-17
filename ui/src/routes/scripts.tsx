import { createFileRoute } from "@tanstack/react-router";
import { useConfig, useScripts } from "@/lib/hooks";
import { ErrorState, LoadingState, PageHeader } from "@/components/page-state";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import type {
  FogConfig,
  ProxyConfig,
  ProxyRoute,
  ScriptConfig,
} from "@/lib/api";

export const Route = createFileRoute("/scripts")({
  component: ScriptsPage,
});

function ConcurrencyBadge({ concurrent }: { concurrent: boolean }) {
  return (
    <Badge
      variant="outline"
      className={cn(
        "gap-1.5 font-mono",
        concurrent
          ? "border-primary/30 bg-primary/15 text-primary"
          : "border-border text-muted-foreground"
      )}
    >
      <span
        className={cn(
          "size-1.5 rounded-full",
          concurrent ? "bg-primary" : "bg-muted-foreground"
        )}
      />
      {concurrent ? "concurrent" : "sequential"}
    </Badge>
  );
}

function RouteRow({ route }: { route: ProxyRoute }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 py-1.5 font-mono text-xs">
      <span className="text-foreground/90">{route.path}</span>
      <span className="text-muted-foreground">→</span>
      <span className="text-primary">{route.upstream}</span>
      <span className="ml-auto flex items-center gap-1.5">
        <span className="text-muted-foreground">host: {route.host ?? "*"}</span>
        {route.ws === true ? (
          <Badge
            variant="outline"
            className="border-info/30 bg-info/15 text-info px-1.5 py-0 text-[10px]"
          >
            ws
          </Badge>
        ) : route.ws === false ? (
          <Badge
            variant="outline"
            className="text-muted-foreground px-1.5 py-0 text-[10px]"
          >
            http
          </Badge>
        ) : null}
      </span>
    </div>
  );
}

/** Dark terminal panel rendering a script's proxy block (port + routes). */
function ProxyPanel({ proxy }: { proxy: ProxyConfig }) {
  const keyFor = (route: ProxyRoute) =>
    `${route.path}|${route.host ?? "*"}|${route.upstream}|${String(route.ws)}`;

  return (
    <div className="terminal-surface overflow-hidden rounded-md">
      <div className="border-border/60 flex items-center justify-between border-b px-3 py-1.5">
        <span className="text-muted-foreground font-mono text-[11px] tracking-wider uppercase">
          proxy
        </span>
        <span className="text-primary font-mono text-[11px]">
          :{proxy.port}
        </span>
      </div>
      <div className="px-3 py-1">
        {proxy.routes.length === 0 ? (
          <p className="text-muted-foreground py-1.5 font-mono text-xs">
            No routes configured.
          </p>
        ) : (
          proxy.routes.map((route) => (
            <RouteRow key={keyFor(route)} route={route} />
          ))
        )}
      </div>
    </div>
  );
}

function ScriptCard({
  name,
  concurrent,
  services,
  proxy,
}: ScriptConfig & { name: string }) {
  return (
    <Card className="gap-3 py-4">
      <CardHeader className="gap-1.5 px-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle className="font-mono text-base">{name}</CardTitle>
          <ConcurrencyBadge concurrent={concurrent} />
        </div>
        {proxy ? (
          <CardDescription className="font-mono">
            proxy :{proxy.port}
          </CardDescription>
        ) : (
          <CardDescription className="font-mono">
            no proxy exposed
          </CardDescription>
        )}
      </CardHeader>
      <CardContent className="flex flex-col gap-3 px-5">
        <div>
          <span className="text-muted-foreground font-mono text-[11px] tracking-wider uppercase">
            services
          </span>
          {services.length > 0 ? (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {services.map((svc) => (
                <Badge key={svc} variant="outline" className="font-mono">
                  {svc}
                </Badge>
              ))}
            </div>
          ) : (
            <p className="text-muted-foreground mt-1 font-mono text-xs">none</p>
          )}
        </div>
        {proxy ? <ProxyPanel proxy={proxy} /> : null}
      </CardContent>
    </Card>
  );
}

/** Compact summary of the global fog config, shown above the script grid. */
function ConfigSummary({ config }: { config: FogConfig }) {
  const rows = [
    {
      label: "theme",
      value: config.theme ? "dark" : "light",
    },
    {
      label: "max scrollback",
      value:
        config.max_scrollback === null
          ? "unlimited"
          : `${config.max_scrollback} lines`,
    },
    {
      label: "sidebar",
      value: config.sidebar
        ? `${config.sidebar.min_width}–${config.sidebar.max_width}px`
        : "unset",
    },
    {
      label: "dnsmasq",
      value: config.dnsmasq
        ? `${config.dnsmasq.address}:${config.dnsmasq.port}${
            config.dnsmasq.domains.length > 0
              ? ` · ${config.dnsmasq.domains.join(", ")}`
              : ""
          }`
        : "unset",
    },
    {
      label: "router",
      value: config.router
        ? `${config.router.shared_network} · :${config.router.index_port} · ${
            config.router.tls_enabled ? "TLS" : "no TLS"
          }`
        : "unset",
    },
  ];

  return (
    <Card className="gap-3 py-4">
      <CardHeader className="gap-0.5 px-5">
        <CardTitle className="font-mono text-sm">config</CardTitle>
        <CardDescription className="font-mono text-xs">
          {config.scripts.length} script
          {config.scripts.length === 1 ? "" : "s"} defined
        </CardDescription>
      </CardHeader>
      <CardContent className="px-5">
        <dl className="grid grid-cols-1 gap-x-8 gap-y-0 md:grid-cols-2">
          {rows.map((row) => (
            <div
              key={row.label}
              className="border-border/40 flex items-baseline justify-between gap-3 border-b py-1"
            >
              <dt className="text-muted-foreground font-mono text-[11px] tracking-wider uppercase">
                {row.label}
              </dt>
              <dd className="text-foreground/90 truncate text-right font-mono text-xs">
                {row.value}
              </dd>
            </div>
          ))}
        </dl>
      </CardContent>
    </Card>
  );
}

function ScriptsPage() {
  const scriptsQuery = useScripts();
  const configQuery = useConfig();

  const entries = Object.entries(scriptsQuery.data?.scripts ?? {}).sort(
    ([a], [b]) => a.localeCompare(b)
  );

  return (
    <div className="space-y-6">
      <PageHeader
        title="Scripts & Config"
        description="Named profiles that start a set of services, each with its own proxy and routes."
      />

      {/* Config summary is context — never blocks the scripts list. */}
      {configQuery.data ? (
        <ConfigSummary config={configQuery.data.config} />
      ) : configQuery.isLoading ? (
        <Card className="gap-3 py-4">
          <CardContent className="px-5">
            <Skeleton className="h-4 w-32" />
            <Skeleton className="mt-3 h-4 w-full" />
            <Skeleton className="mt-2 h-4 w-2/3" />
          </CardContent>
        </Card>
      ) : null}

      {scriptsQuery.isLoading ? (
        <LoadingState label="Loading scripts…" />
      ) : scriptsQuery.isError ? (
        <ErrorState message={scriptsQuery.error?.message} />
      ) : entries.length === 0 ? (
        <Card>
          <CardContent className="text-muted-foreground py-8 text-center font-mono text-sm">
            No scripts configured.
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {entries.map(([name, script]) => (
            <ScriptCard key={name} name={name} {...script} />
          ))}
        </div>
      )}
    </div>
  );
}
