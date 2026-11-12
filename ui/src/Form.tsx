// The generic pages a desk keeps reachable: a record's own form (with a strip to its desk when it
// has one, so no action of the form is lost) and an entity's plain list.

import { useParams } from "react-router";
import { FormView, ListView } from "@ferrum/records";
import { DeskTabs } from "./DeskTabs";
import { isDesk } from "./desks";

export function FormPage() {
  const { entity = "", id = "" } = useParams();
  return (
    <>
      {id !== "new" && isDesk(entity, "record") && <DeskTabs entity={entity} id={id} current="form" />}
      <FormView />
    </>
  );
}

export function ListPage() {
  return <ListView />;
}
