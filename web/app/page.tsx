import type { Metadata } from "next";
import { Studio } from "./studio";

export const metadata: Metadata = {
  title: "OMAR Mission Control",
  description: "Build and observe principled OMAR workflows.",
};

export default function Home() {
  // Mode is chosen when Mission Control is launched, not from the UI: an
  // address means live, absent means the offline demo topology.
  return <Studio serveUrl={process.env.OMAR_SERVE_URL ?? ""} />;
}
