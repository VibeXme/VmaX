import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import appConfig from "../../src-tauri/tauri.conf.json";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Minimize2, Minus, Settings, Square, X } from "lucide-react";
import { VmaXLogo } from "./VmaXLogo";

interface TitleBarProps {
  settingsOpen: boolean;
  onOpenSettings: () => void;
}

function getTauriWindow() {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
    return null;
  }
  return getCurrentWindow();
}

function isMobileRuntime(): boolean {
  if (typeof navigator === "undefined") {
    return false;
  }
  return /Android|iPhone|iPad|Mobile/i.test(navigator.userAgent);
}

export function TitleBar({ settingsOpen, onOpenSettings }: TitleBarProps) {
  const [version, setVersion] = useState(appConfig.version);
  const [isMaximized, setIsMaximized] = useState(false);
  const mobile = isMobileRuntime();

  useEffect(() => {
    if (getTauriWindow()) void getVersion().then(setVersion).catch(() => {});
  }, []);
  useEffect(() => {
    const appWindow = getTauriWindow();
    if (!appWindow) {
      return;
    }

    let cancelled = false;
    appWindow
      .isMaximized()
      .then((max) => {
        if (!cancelled) {
          setIsMaximized(max);
        }
      })
      .catch(() => {});

    const unlistenPromise = appWindow.onResized(() => {
      appWindow
        .isMaximized()
        .then((max) => {
          if (!cancelled) {
            setIsMaximized(max);
          }
        })
        .catch(() => {});
    });

    return () => {
      cancelled = true;
      unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  async function handleMinimize() {
    const appWindow = getTauriWindow();
    if (appWindow) {
      await appWindow.minimize().catch(() => {});
    }
  }

  async function handleToggleMaximize() {
    const appWindow = getTauriWindow();
    if (appWindow) {
      await appWindow.toggleMaximize().catch(() => {});
    }
  }

  async function handleClose() {
    const appWindow = getTauriWindow();
    if (appWindow) {
      await appWindow.close().catch(() => {});
    }
  }

  return (
    <div className={`sticky top-0 z-40 shrink-0 border-b border-zinc-800/90 bg-zinc-950/95 backdrop-blur flex items-center justify-between pl-4 pr-2 select-none ${mobile ? "h-[calc(3rem+env(safe-area-inset-top))] pt-[env(safe-area-inset-top)]" : "h-10"}`}>
      {/* Draggable Title & Spacer Region */}
      <div
        data-tauri-drag-region
        className="flex-1 h-full flex items-center gap-2.5 min-w-0 cursor-default"
      >
        <span className="pointer-events-none flex items-center">
          <VmaXLogo size={18} />
        </span>
        <span className="pointer-events-none text-xs font-semibold tracking-wider text-zinc-200 uppercase">
          VmaX
        </span>
        <span className="pointer-events-none truncate rounded border border-zinc-700 px-1.5 font-mono text-[10px] text-zinc-300" aria-label={`Build version ${version}`}>v{version}</span>
      </div>

      {/* Right Action Strip: Settings Gear + Window Controls */}
      <div className="flex items-center gap-1 shrink-0">
        <button
          type="button"
          onClick={onOpenSettings}
          title="Settings"
          aria-label="Open settings"
          className={`flex ${mobile ? "h-11 w-11" : "h-7 w-8"} items-center justify-center rounded-md transition-colors cursor-pointer ${
            settingsOpen
              ? "bg-zinc-800 text-white"
              : "text-zinc-400 hover:bg-zinc-900 hover:text-white"
          }`}
        >
          <Settings className="h-3.5 w-3.5" />
        </button>

        {!mobile && (
          <>
            <div className="mx-1 h-4 w-px bg-zinc-800" aria-hidden="true" />

            <button
              type="button"
              onClick={handleMinimize}
              title="Minimize"
              aria-label="Minimize window"
              className="flex h-7 w-8 items-center justify-center rounded-md text-zinc-400 hover:bg-zinc-900 hover:text-white transition-colors cursor-pointer"
            >
              <Minus className="h-3.5 w-3.5" />
            </button>

            <button
              type="button"
              onClick={handleToggleMaximize}
              title={isMaximized ? "Restore" : "Maximize"}
              aria-label={isMaximized ? "Restore window" : "Maximize window"}
              className="flex h-7 w-8 items-center justify-center rounded-md text-zinc-400 hover:bg-zinc-900 hover:text-white transition-colors cursor-pointer"
            >
              {isMaximized ? (
                <Minimize2 className="h-3.5 w-3.5" />
              ) : (
                <Square className="h-3 w-3" />
              )}
            </button>

            <button
              type="button"
              onClick={handleClose}
              title="Close"
              aria-label="Close window"
              className="flex h-7 w-8 items-center justify-center rounded-md text-zinc-400 hover:bg-zinc-800 hover:text-white transition-colors cursor-pointer"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </>
        )}
      </div>
    </div>
  );
}
