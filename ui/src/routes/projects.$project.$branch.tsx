import { createFileRoute, Outlet } from "@tanstack/react-router";

export const Route = createFileRoute("/projects/$project/$branch")({
  component: BranchLayout,
});

/** Pass-through layout so branch scripts render into the nested route. */
function BranchLayout() {
  return <Outlet />;
}
