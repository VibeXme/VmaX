import { readFileSync, writeFileSync } from "node:fs";

const manifest = new URL(
  "../src-tauri/gen/android/app/src/main/AndroidManifest.xml",
  import.meta.url,
);
const source = readFileSync(manifest, "utf8");
const permissions = [
  ["uses-permission", "android.permission.CAMERA", ""],
  ["uses-feature", "android.hardware.camera", ' android:required="false"'],
  ["uses-feature", "android.hardware.camera.autofocus", ' android:required="false"'],
];

const missing = permissions
  .filter(([element, name]) => {
    const tags = source.match(new RegExp(`<${element}\\s+[^>]*>`, "g")) ?? [];
    return !tags.some((tag) => tag.includes(`"${name}"`) || tag.includes(`'${name}'`));
  })
  .map(([element, name, attributes]) => `    <${element} android:name="${name}"${attributes} />`);

if (missing.length) {
  const updated = source.replace(/<manifest\b[^>]*>/, (root) => {
    const newline = source.includes("\r\n") ? "\r\n" : "\n";
    return [root, ...missing].join(newline);
  });
  if (updated === source) throw new Error("Android manifest root element is missing");
  writeFileSync(manifest, updated);
}
