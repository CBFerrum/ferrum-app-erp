// The box everything starts from. A hardware scanner types a code and presses Enter, so it needs no
// clicks; on a phone it is one big field. It keeps its focus, so the next scan just works.

import { type FormEvent, useEffect, useId, useRef, useState } from "react";
import { Button } from "@ferrum/kit";
import { cleanScan } from "./logic";

export function ScanBox({ label, onScan, busy, keepFocus }: { label: string; onScan: (code: string) => void; busy?: boolean; keepFocus?: boolean }) {
  const id = useId();
  const box = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState("");
  useEffect(() => {
    if (keepFocus !== false) box.current?.focus();
  }, [keepFocus, busy]);
  const send = (e: FormEvent) => {
    e.preventDefault();
    const code = cleanScan(value);
    if (code) {
      onScan(code);
      setValue("");
    }
  };
  return (
    <form className="wms-scan" onSubmit={send}>
      <label htmlFor={id}>{label}</label>
      <div className="wms-scan-row">
        <input id={id} ref={box} className="input" value={value} autoComplete="off" autoCapitalize="characters" enterKeyHint="go" onChange={(e) => setValue(e.target.value)} />
        <Button type="submit" variant="primary" loading={busy}>
          OK
        </Button>
      </div>
    </form>
  );
}
