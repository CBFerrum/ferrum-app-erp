// The shop floor's own screens as the kit's tabs: the same tab strip, keys and look as every record workspace.

import type { ReactNode } from "react";
import { useNavigate } from "react-router";
import { PageHeader, TabPanel, Tabs } from "@ferrum/kit";

const SECTIONS = [
  { id: "floor", label: "Floor", to: "/", says: "What is running where, and what is ready to go." },
  { id: "oee", label: "OEE", to: "/oee", says: "How well one machine was used in a period: availability, speed and quality." },
  { id: "trace", label: "Trace a lot", to: "/trace", says: "Where a lot of material went, and what went into a product." },
] as const;

export type Section = (typeof SECTIONS)[number]["id"];

/** A screen's frame: the kit's page head, the section tabs, then the section. */
export function Screen({ section, actions, children }: { section: Section; actions?: ReactNode; children: ReactNode }) {
  const go = useNavigate();
  return (
    <div>
      <PageHeader title="Shop floor" description={SECTIONS.find((s) => s.id === section)?.says} actions={actions} />
      <Tabs id="mes" tabs={SECTIONS.map(({ id, label }) => ({ id, label }))} value={section} onChange={(id) => go(SECTIONS.find((s) => s.id === id)?.to ?? "/")} />
      <TabPanel id="mes" tab={section}>
        <div className="mes-page">{children}</div>
      </TabPanel>
    </div>
  );
}
