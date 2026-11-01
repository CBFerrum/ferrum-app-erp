// Whether the window is a phone's width: the screens that show two panes side by side show one at a
// time there, and the pane in use is in the address.

import { useEffect, useState } from "react";

const QUERY = "(max-width: 880px)";

export function useNarrow(): boolean {
  const [narrow, setNarrow] = useState(() => typeof window !== "undefined" && window.matchMedia(QUERY).matches);
  useEffect(() => {
    const m = window.matchMedia(QUERY);
    const on = () => setNarrow(m.matches);
    on();
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);
  return narrow;
}
