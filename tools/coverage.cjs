const fs = require("fs");
const dir = "ui/windows";
const files = fs.readdirSync(dir).filter((f) => f.endsWith(".slint")).map((f) => dir + "/" + f);
const msgids = new Set();
for (const f of files) {
  const t = fs.readFileSync(f, "utf8");
  const re = /@tr\("((?:[^"\\]|\\.)*)"/g;
  let m;
  while ((m = re.exec(t))) msgids.add(m[1].replace(/\\"/g, '"'));
}
const po = fs.readFileSync("lang/pt/LC_MESSAGES/clipo.po", "utf8");
const poids = new Set([...po.matchAll(/^msgid "((?:[^"\\]|\\.)*)"/gm)].map((m) => m[1].replace(/\\"/g, '"')));
let hit = 0;
const miss = [];
for (const id of msgids) (poids.has(id) ? hit++ : miss.push(id));
console.log(`@tr strings: ${msgids.size} | translated(pt): ${hit} | English fallback: ${miss.length}`);
console.log("--- English fallback (no pt translation) ---");
miss.forEach((s) => console.log("  " + JSON.stringify(s)));
