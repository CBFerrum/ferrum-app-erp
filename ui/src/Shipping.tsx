// The shipping desk (src/shipping.rs). A shipment's parcels, the rates its carrier answers with, the
// rate chosen, the label bought once (the button is disabled while it is sent and the request carries
// its key, as the server's call to the carrier does), the label's file, tracking and cancelling the
// label; and the carrier connections with the state of the platform connection each one's calls go
// through, which is read only here and managed in Administration.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, download, errorMessage, formatDateTime, isolate, newKey, t } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, StatePanel, toast, useShellTitle } from "@ferrum/kit";
import { LinkTitle } from "@ferrum/records";
import { Meta, PageHead } from "./chrome";
import { act, Gate, useRights } from "./desk";
import { DeskTabs } from "./DeskTabs";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { cheapest, chooseBody, connectionOf, may, type PlatformConnection, type Rate, type Shipment, TONE } from "./shipments";

const STATUS = (s: string) =>
  s === "Draft" ? t("Draft") : s === "Rated" ? t("Rates offered") : s === "Label Bought" ? t("Label bought") : s === "In Transit" ? t("In transit") : s === "Delivered" ? t("Delivered") : s === "Exception" ? t("Problem") : s === "Cancelled" ? t("Cancelled") : isolate(s);
const SIGN_IN = (a: string | undefined) => (a === "ok" ? t("Working") : a === "failing" ? t("Failing") : a === "untested" ? t("Not tested") : t("No credentials"));

/** `/desk/shipping`: the shipments and the carrier connections. */
export function ShippingHome() {
  useShellTitle(t("Shipping"));
  return (
    <Gate entity="CarrierShipment">
      <Home />
    </Gate>
  );
}

/** `/desk/shipping/:id`: one shipment. */
export function ShipmentDesk() {
  const { id = "" } = useParams();
  useShellTitle(t("Shipment"));
  return (
    <Gate entity="CarrierShipment">
      <Desk id={id} />
    </Gate>
  );
}

interface Row {
  id: string;
  number?: string | null;
  carrier: string;
  reference?: string | null;
  service?: string | null;
  tracking_number?: string | null;
  status: Shipment["status"];
}
interface Carrier {
  id: string;
  connection_name: string;
  enabled: boolean;
  test?: boolean;
  connection?: string | null;
}

function Home() {
  const ships = useQuery({ queryKey: ["erp", "shipments"], queryFn: ({ signal }) => api.get<{ rows: Row[] }>(docPath("CarrierShipment"), { fields: "id,number,carrier,reference,service,tracking_number,status", sort: "created_at desc", limit: 50 }, signal) });
  return (
    <div className="erp-page">
      <PageHead
        title={t("Shipping")}
        meta={<Meta items={[t("Parcels, rates, labels and tracking, through the carriers' connections.")]} />}
        actions={<Link className="btn btn-primary btn-md" to="/e/CarrierShipment/new">{t("New shipment")}</Link>}
      />
      {ships.error ? (
        <LoadFailed error={ships.error} onRetry={() => void ships.refetch()} title={t("Could not load the shipments")} />
      ) : !ships.data ? (
        <Loading columns={5} label={t("Loading the shipments")} />
      ) : ships.data.rows.length === 0 ? (
        <StatePanel kind="empty" title={t("No shipment yet")}>{t("Ship by carrier from a delivery note, or make a shipment here with its parcels.")}</StatePanel>
      ) : (
        <div className="table-wrap">
          <table className="table" aria-label={t("Shipments")}>
            <thead>
              <tr>
                <th scope="col">{t("Shipment")}</th>
                <th scope="col">{t("Carrier")}</th>
                <th scope="col">{t("Reference")}</th>
                <th scope="col">{t("Service")}</th>
                <th scope="col">{t("Tracking number")}</th>
                <th scope="col">{t("State")}</th>
              </tr>
            </thead>
            <tbody>
              {ships.data.rows.map((r) => (
                <tr key={r.id}>
                  <td><Link to={`/desk/shipping/${r.id}`}>{isolate(r.number ?? r.id)}</Link></td>
                  <td><LinkTitle to="CarrierConnection" id={r.carrier} /></td>
                  <td>{isolate(r.reference ?? "")}</td>
                  <td>{isolate(r.service ?? "")}</td>
                  <td>{isolate(r.tracking_number ?? "")}</td>
                  <td><Badge tone={TONE[r.status] ?? "neutral"}>{STATUS(r.status)}</Badge></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <Carriers />
    </div>
  );
}

function Carriers() {
  const rights = useRights("CarrierConnection");
  const list = useQuery({ queryKey: ["erp", "carriers"], enabled: rights.may("read"), queryFn: ({ signal }) => api.get<{ rows: Carrier[] }>(docPath("CarrierConnection"), { fields: "id,connection_name,enabled,test,connection", limit: 50 }, signal) });
  // Administration's list of connections: a person who may not read it sees the carrier without its state.
  const conns = useQuery({ queryKey: ["erp", "platform-connections"], retry: false, queryFn: ({ signal }) => api.get<{ connections: PlatformConnection[] }>("/api/admin/connections", undefined, signal) });
  if (!rights.may("read")) return null;
  return (
    <section className="erp-panel" aria-labelledby="sh-carriers">
      <div className="erp-panel-head">
        <h2 id="sh-carriers" className="erp-card-title">{t("Carrier connections")}</h2>
        <a href="/admin/?tab=connections">{t("Connections in Administration")}</a>
      </div>
      {list.error ? (
        <LoadFailed error={list.error} onRetry={() => void list.refetch()} title={t("Could not load the carriers")} />
      ) : !list.data ? (
        <Loading columns={4} rows={2} label={t("Loading the carriers")} />
      ) : list.data.rows.length === 0 ? (
        <p className="muted">{t("No carrier is connected yet.")}</p>
      ) : (
        <div className="table-wrap">
          <table className="table" aria-label={t("Carrier connections")}>
            <thead>
              <tr>
                <th scope="col">{t("Carrier")}</th>
                <th scope="col">{t("Connection")}</th>
                <th scope="col">{t("Sign-in")}</th>
                <th scope="col">{t("State")}</th>
              </tr>
            </thead>
            <tbody>
              {list.data.rows.map((c) => {
                const conn = connectionOf(conns.data?.connections, c.connection);
                return (
                  <tr key={c.id}>
                    <td><Link to={`/e/CarrierConnection/${c.id}`}>{isolate(c.connection_name)}</Link> {c.test && <Badge>{t("Test carrier")}</Badge>}</td>
                    <td>{c.connection ? isolate(conn?.name ?? c.connection) : <span className="muted">{t("Not connected yet")}</span>}</td>
                    <td>{conn ? <Badge tone={conn.health?.authentication === "ok" ? "green" : conn.health?.authentication === "failing" ? "red" : "amber"}>{SIGN_IN(conn.health?.authentication)}</Badge> : ""}</td>
                    <td>
                      <Badge tone={c.enabled ? "green" : "neutral"}>{c.enabled ? t("On") : t("Off")}</Badge>
                      {conn?.last_error && <p className="erp-negative">{isolate(conn.last_error)}</p>}
                      {!!conn?.health?.dead_letters && <p className="muted">{t("{count} calls failed", { count: conn.health.dead_letters })}</p>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function Desk({ id }: { id: string }) {
  const qc = useQueryClient();
  const r = useRights("CarrierShipment");
  const [busy, setBusy] = useState<null | "rate" | "choose" | "buy" | "track" | "cancel">(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [pick, setPick] = useState<string | null>(null);
  // One key for the label this shipment buys: a second press, or a retry after a lost answer, buys one.
  const buyKey = useRef(newKey());
  const q = useQuery({ queryKey: ["erp", "shipment", id], queryFn: ({ signal }) => api.get<Shipment>(docPath("CarrierShipment", id), undefined, signal) });
  const s = q.data;
  const files = useQuery({
    queryKey: ["erp", "shipment-files", id, s?.label_file],
    enabled: !!s?.label_file,
    queryFn: ({ signal }) => api.get<{ attachments: { id: string; file_name: string; url: string }[] }>(docPath("CarrierShipment", id, "attachments"), undefined, signal),
  });

  const refresh = async () => {
    const fresh = await api.get<Shipment>(docPath("CarrierShipment", id));
    qc.setQueryData(["erp", "shipment", id], fresh);
  };
  const run = async (what: NonNullable<typeof busy>, go: () => Promise<unknown>, done: string) => {
    if (busy) return;
    setBusy(what);
    setProblem(null);
    try {
      await go();
      await refresh();
      toast(done);
    } catch (e) {
      setProblem(errorMessage(e));
      // What is true now, after a refusal: the shipment may have moved on.
      void refresh().catch(() => undefined);
    } finally {
      setBusy(null);
    }
  };

  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the shipment")} />;
  if (!s) return <Loading label={t("Opening the shipment")} />;
  const rates = s.rates ?? [];
  const best = cheapest(rates);
  const write = r.may("write");
  const chosen = pick ?? s.service ?? null;

  const buy = async () => {
    const ok = await confirmDialog({ title: t("Buy the label?"), message: t("The carrier charges for it. A second press does not buy another."), confirmLabel: t("Buy the label") });
    if (ok) await run("buy", () => act("CarrierShipment", id, "buy_label", {}, buyKey.current), t("Label bought"));
  };
  const cancel = async () => {
    const ok = await confirmDialog({ title: t("Cancel the label?"), message: t("The label is cancelled with the carrier, and its cost is taken back."), confirmLabel: t("Cancel the label"), danger: true });
    if (ok) {
      await run("cancel", () => act("CarrierShipment", id, "cancel_label", {}, newKey()), t("Label cancelled"));
      buyKey.current = newKey();
    }
  };

  return (
    <div className="erp-page">
      <DeskTabs entity="CarrierShipment" id={id} current="desk" />
      <PageHead
        title={s.number ? t("Shipment {number}", { number: s.number }) : t("Shipment")}
        meta={
          <Meta
            items={[
              <Badge key="s" tone={TONE[s.status] ?? "neutral"}>{STATUS(s.status)}</Badge>,
              <LinkTitle key="c" to="CarrierConnection" id={s.carrier} />,
              s.reference && isolate(s.reference),
            ]}
          />
        }
        actions={
          <>
            <Link className="btn btn-ghost btn-md" to="/desk/shipping">{t("All shipments")}</Link>
          </>
        }
      />
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <section className="erp-panel" aria-labelledby="sh-parcels">
        <h2 id="sh-parcels" className="erp-card-title">{t("Parcels")}</h2>
        {s.packages.length === 0 ? (
          <p className="muted">{t("This shipment has no parcel yet. Add its parcels in the record.")}</p>
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th scope="col">{t("Parcel")}</th>
                  <th scope="col" className="num">{t("Weight")}</th>
                  <th scope="col" className="num">{t("Length")}</th>
                  <th scope="col" className="num">{t("Width")}</th>
                  <th scope="col" className="num">{t("Height")}</th>
                  <th scope="col">{t("Contents")}</th>
                </tr>
              </thead>
              <tbody>
                {s.packages.map((p, i) => (
                  <tr key={p.id ?? i}>
                    <td>{isolate(p.reference ?? String(i + 1))}</td>
                    <td className="num">{isolate(show(p.weight))}</td>
                    <td className="num">{isolate(show(p.length))}</td>
                    <td className="num">{isolate(show(p.width))}</td>
                    <td className="num">{isolate(show(p.height))}</td>
                    <td>{isolate(p.contents ?? "")}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="erp-panel" aria-labelledby="sh-rates">
        <div className="erp-panel-head">
          <h2 id="sh-rates" className="erp-card-title">{t("Rates")}</h2>
          <Button disabled={!write || !may.rate(s) || busy !== null} loading={busy === "rate"} onClick={() => void run("rate", () => act("CarrierShipment", id, "get_rates", {}), t("Rates received"))}>
            {rates.length ? t("Ask again") : t("Get rates")}
          </Button>
        </div>
        {!may.rate(s) && s.tracking_number ? (
          <p className="muted">{t("A label is bought: cancel it to ship another way.")}</p>
        ) : rates.length === 0 ? (
          <p className="muted">{t("Ask the carrier what it would charge for these parcels. The shipment needs a ship-to address.")}</p>
        ) : (
          <fieldset className="erp-stack" disabled={!may.choose(s) || !write}>
            <legend className="sr-only">{t("Rates offered")}</legend>
            {rates.map((x: Rate) => (
              <label key={x.service} className="check-field">
                <input type="radio" name="sh-rate" checked={chosen === x.service} onChange={() => setPick(x.service)} />
                <span>
                  {isolate(x.service)} · {isolate(show(x.amount))} {x.currency ? isolate(x.currency) : ""}
                  {x.days != null && <> · {t("{days} days", { days: x.days })}</>}
                  {best?.service === x.service && rates.length > 1 && <> · <Badge tone="green">{t("Cheapest")}</Badge></>}
                </span>
              </label>
            ))}
            <div className="erp-actions">
              <Button disabled={!chosen || chosen === s.service || busy !== null} loading={busy === "choose"} onClick={() => void run("choose", () => act("CarrierShipment", id, "choose_rate", chooseBody(chosen!)), t("Rate chosen"))}>
                {t("Choose this rate")}
              </Button>
            </div>
          </fieldset>
        )}
      </section>

      <section className="erp-panel" aria-labelledby="sh-label">
        <h2 id="sh-label" className="erp-card-title">{t("Label")}</h2>
        {s.tracking_number ? (
          <div className="erp-stack">
            <p>
              {t("Tracking number")} {isolate(s.tracking_number)}{" "}
              {s.tracking_url && <a href={s.tracking_url} target="_blank" rel="noopener noreferrer">{t("Track with the carrier")}</a>}
            </p>
            {s.cost && <p className="muted">{t("Label cost {cost}", { cost: show(s.cost) })}</p>}
            {files.data?.attachments.map((f) => (
              <Button key={f.id} icon="printer" onClick={() => download(f.url, f.file_name).catch((e) => toast(errorMessage(e), "error"))}>
                {t("Label file: {name}", { name: f.file_name })}
              </Button>
            ))}
            {files.error && <LoadFailed error={files.error} onRetry={() => void files.refetch()} title={t("Could not load the label file")} />}
          </div>
        ) : (
          <p className="muted">{s.service ? t("{service} is chosen. Buying the label is charged by the carrier.", { service: s.service }) : t("Choose a rate first.")}</p>
        )}
        <div className="erp-actions">
          {!s.tracking_number && (
            <Button variant="primary" disabled={!write || !may.buy(s) || busy !== null} loading={busy === "buy"} onClick={() => void buy()}>
              {t("Buy the label")}
            </Button>
          )}
          {may.cancel(s) && (
            <Button disabled={!write || busy !== null} loading={busy === "cancel"} onClick={() => void cancel()}>
              {t("Cancel the label")}
            </Button>
          )}
        </div>
      </section>

      {may.track(s) && (
        <section className="erp-panel" aria-labelledby="sh-track">
          <div className="erp-panel-head">
            <h2 id="sh-track" className="erp-card-title">{t("Tracking")}</h2>
            <Button disabled={busy !== null} loading={busy === "track"} onClick={() => void run("track", () => act("CarrierShipment", id, "track", {}), t("Tracking updated"))}>
              {t("Track now")}
            </Button>
          </div>
          <p>{s.tracking_status ? isolate(s.tracking_status) : <span className="muted">{t("The carrier has not said anything yet.")}</span>}</p>
          {s.last_tracked_at && <p className="muted">{t("Asked {when}", { when: formatDateTime(s.last_tracked_at) })}</p>}
          {s.delivered_at && <p>{t("Delivered {when}", { when: formatDateTime(s.delivered_at) })}</p>}
        </section>
      )}
    </div>
  );
}
