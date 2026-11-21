import { copyFileSync, readFileSync, writeFileSync } from "node:fs";

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

const activity = new URL("./java/com/vmax/app/MainActivity.kt", manifest);
const decoder = new URL("./java/com/vmax/app/VmaxLowLatencyDecoder.kt", manifest);
const activitySource = readFileSync(activity, "utf8");
const marker = "// VmaX native video";
if (!activitySource.includes(marker)) {
  const hook = activitySource.includes("override fun onWebViewCreate");
  let updated;
  if (hook) {
    const method = /override fun onWebViewCreate\(\s*(\w+)\s*:\s*(?:android\.webkit\.)?WebView\s*\)\s*\{/;
    updated = activitySource.replace(method, (line, webView) =>
      `${line}\n    ${marker}\n    VmaxLowLatencyDecoder.attach(this, ${webView})`);
  } else {
    const method = `\n    ${marker}\n    override fun onWebViewCreate(webView: android.webkit.WebView) {\n        super.onWebViewCreate(webView)\n        VmaxLowLatencyDecoder.attach(this, webView)\n    }\n`;
    const declaration = /class MainActivity\s*:\s*TauriActivity\(\)/;
    updated = activitySource.replace(new RegExp(declaration.source + "(\\s*\\{)?"), (line, brace) =>
      brace ? line + method : `${line} {${method}}\n`);
  }
  if (!updated.includes(marker)) throw new Error("Could not install the native video hook in MainActivity.kt");
  writeFileSync(activity, updated);
}
copyFileSync(new URL("./VmaxLowLatencyDecoder.kt", import.meta.url), decoder);
