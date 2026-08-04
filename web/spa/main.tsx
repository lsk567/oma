import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import "../app/globals.css";
import { Studio } from "../app/studio";

/**
 * Mode is chosen at launch in the Worker build, because only the server knows
 * the daemon's address there. Served by `omar serve` there is nothing to
 * choose: the page came from the daemon, so the daemon is where it came from.
 * Same origin, which is also why none of this needs CORS.
 */
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Studio serveUrl={window.location.origin} />
  </StrictMode>,
);
