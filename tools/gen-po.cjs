// Regenerate the gettext .po catalogs for the native app.
//
// Self-contained and native-sourced: the set of translatable strings is the
// `@tr("…")` literals in ui/**/*.slint — nothing else. Existing translations
// are read back from the current .po (preserved verbatim), strings the UI no
// longer uses are dropped, and brand-new strings are merged from NEW below.
// Strings with no translation (typographic terms, PNG/JPG, "30 fps", "#hex",
// "Menu") are simply left out → Slint falls back to the English msgid.
//
//   node tools/gen-po.cjs
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const UI = path.join(ROOT, "ui");
const LANGDIR = path.join(ROOT, "lang");
const LANGS = ["pt", "es", "fr", "de", "it", "ja", "ko", "zh", "ru", "hi", "ar"];

// New strings not yet present in any .po (values in LANGS order).
const NEW = {
  "Update now": ["Atualizar agora", "Actualizar ahora", "Mettre à jour", "Jetzt aktualisieren", "Aggiorna ora", "今すぐ更新", "지금 업데이트", "立即更新", "Обновить", "अभी अपडेट करें", "تحديث الآن"],
  "Installing…": ["Instalando…", "Instalando…", "Installation…", "Installiere…", "Installazione…", "インストール中…", "설치 중…", "正在安装…", "Установка…", "इंस्टॉल हो रहा है…", "جارٍ التثبيت…"],
  "Checking…": ["Verificando…", "Comprobando…", "Vérification…", "Prüfe…", "Controllo…", "確認中…", "확인 중…", "正在检查…", "Проверка…", "जाँच हो रही है…", "جارٍ التحقق…"],
  "Copied": ["Copiado", "Copiado", "Copié", "Kopiert", "Copiato", "コピーしました", "복사됨", "已复制", "Скопировано", "कॉपी किया गया", "تم النسخ"],
};

const decode = (s) => s.replace(/\\(.)/g, (_, c) => (c === "n" ? "\n" : c === "t" ? "\t" : c));
const poEsc = (s) => s.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n");

// Every @tr("…") literal across the Slint UI → set of decoded msgids.
function uiStrings() {
  const set = new Set();
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith(".slint")) {
        const txt = fs.readFileSync(p, "utf8");
        const re = /@tr\("((?:[^"\\]|\\.)*)"/g;
        let m;
        while ((m = re.exec(txt))) set.add(decode(m[1]));
      }
    }
  };
  walk(UI);
  return set;
}

// Parse a .po into decoded msgid → raw (still-escaped) msgstr, preserving the
// existing translation text exactly.
function parsePo(file) {
  const txt = fs.readFileSync(file, "utf8");
  const map = new Map();
  const re = /^msgid "((?:[^"\\]|\\.)*)"\nmsgstr "((?:[^"\\]|\\.)*)"/gm;
  let m;
  while ((m = re.exec(txt))) {
    const id = decode(m[1]);
    if (id !== "") map.set(id, m[2]); // skip the "" header
  }
  return map;
}

const ui = uiStrings();
console.log("ui @tr strings:", ui.size);

for (let li = 0; li < LANGS.length; li++) {
  const lang = LANGS[li];
  const file = path.join(LANGDIR, lang, "LC_MESSAGES", "clipo.po");
  const existing = fs.existsSync(file) ? parsePo(file) : new Map();

  let po = 'msgid ""\nmsgstr ""\n"Content-Type: text/plain; charset=UTF-8\\n"\n\n';
  let n = 0;
  for (const id of ui) {
    let raw = existing.get(id); // preserve current translation verbatim
    if (raw == null && NEW[id]) raw = poEsc(NEW[id][li]); // or a freshly-added one
    if (raw == null || raw === "" || decode(raw) === id) continue; // untranslated → English fallback
    po += `msgid "${poEsc(id)}"\nmsgstr "${raw}"\n\n`;
    n++;
  }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, po, "utf8");
  console.log(`${lang}: ${n} entries`);
}
