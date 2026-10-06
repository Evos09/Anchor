// Written last by `npm run build`: tells cargo (crates/anchor-backend
// build.rs) to re-embed the bundle.
import { writeFileSync } from "node:fs";
writeFileSync(new URL("../dist/.stamp", import.meta.url), `${Date.now()}\n`);
