import { execFileSync } from "node:child_process";

const [apk, dexdump] = process.argv.slice(2);
if (!apk || !dexdump) throw new Error("Usage: verify-android-decoder.mjs <apk> <dexdump>");

const dex = execFileSync(dexdump, [apk], {
  encoding: "utf8",
  maxBuffer: 64 * 1024 * 1024,
});
const decoder = dex.split(/^Class #\d+/m).find((entry) =>
  /^\s*Class descriptor\s*:\s*'Lcom\/vmax\/app\/VmaxLowLatencyDecoder;'\s*$/m.test(entry)
);
const methods = new Map(
  [...(decoder ?? "").matchAll(
    /^[ \t]*name[ \t]*:[ \t]*'([^']+)'\r?\n[ \t]*type[ \t]*:[ \t]*'([^']+)'\r?\n[ \t]*access[ \t]*:[^\r\n]*\bSTATIC\b/gm,
  )].map(([, name, descriptor]) => [name, descriptor]),
);

for (const [name, descriptor] of [
  ["onNativeVideoFrame", "([BJIIIZ)V"],
  ["layoutSurface", "(Landroid/app/Activity;Landroid/webkit/WebView;IIIIZ)V"],
  ["resetSession", "()V"],
]) {
  if (methods.get(name) !== descriptor) {
    throw new Error(`Release APK is missing the JNI entry point ${name}${descriptor}`);
  }
}
console.log("Android decoder JNI entry points verified in the release APK.");
