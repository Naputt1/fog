import { useInstanceKillState } from "@/lib/hooks";
import { StatusBadge } from "@/components/status-badge";

/**
 * Renders a "killing" badge while an instance is shutting down, nothing
 * otherwise. Lets compact rows (branch/script lists) surface the transitional
 * state without each of them reading the mutation cache directly.
 */
export function InstanceKillingBadge({
  pid,
  script,
}: {
  pid: number;
  script: string;
}) {
  const { killing } = useInstanceKillState(pid, script);
  if (!killing) return null;
  return <StatusBadge status="killing" />;
}
