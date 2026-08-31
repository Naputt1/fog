import { createContext, useContext } from "react";

interface RightSidebarContextValue {
  open: boolean;
  setOpen: (open: boolean) => void;
  enabled: boolean;
  setEnabled: (enabled: boolean) => void;
}

export const RightSidebarContext = createContext<RightSidebarContextValue | null>(null);

export function useRightSidebar() {
  return useContext(RightSidebarContext);
}
