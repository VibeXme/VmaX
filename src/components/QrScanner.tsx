import { useEffect, useRef, useState, type ChangeEvent } from "react";
import { ImageUp, X } from "lucide-react";

function parseScannedInvite(value: string | undefined) {
  const invite = value?.trim();
  if (!invite || invite.length > 4096 || !/^vmax:\/\/join\/[a-z0-9]{4}-[a-z0-9]{4}\?/i.test(invite)) return null;
  try {
    const endpoint = new URL(new URL(invite).searchParams.get("sig") ?? "");
    return endpoint.protocol === "ws:" || endpoint.protocol === "wss:" ? invite : null;
  } catch {
    return null;
  }
}

export function QrScanner({ onClose, onInvite }: { onClose: () => void; onInvite: (invite: string) => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const video = useRef<HTMLVideoElement>(null);
  const callback = useRef(onInvite);
  callback.current = onInvite;
  const active = useRef(false);
  const completed = useRef(false);
  const [cameraReady, setCameraReady] = useState(false);
  const [message, setMessage] = useState("Starting camera…");

  async function decode(source: CanvasImageSource, width: number, height: number, live = false) {
    const { default: jsQR } = await import("jsqr");
    if (!active.current || completed.current) return;
    const canvas = document.createElement("canvas");
    const scale = Math.min(1, (live ? 720 : 1600) / Math.max(width, height));
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (!context) return;
    context.drawImage(source, 0, 0, canvas.width, canvas.height);
    const image = context.getImageData(0, 0, canvas.width, canvas.height);
    const code = jsQR(image.data, image.width, image.height, { inversionAttempts: live ? "dontInvert" : "attemptBoth" });
    const invite = parseScannedInvite(code?.data);
    if (invite) {
      completed.current = true;
      callback.current(invite);
    } else if (code || !live) {
      setMessage(code ? "Use a VmaX invite QR." : "No QR found in this image.");
    }
  }

  useEffect(() => {
    dialog.current?.showModal();
    active.current = true;
    const player = video.current;
    let stream: MediaStream | undefined;
    let timer: number | undefined;
    let cancelled = false;
    async function scanFrame() {
      if (cancelled || completed.current) return;
      if (!document.hidden && player && player.readyState >= 2 && player.videoWidth > 0) {
        try {
          await decode(player, player.videoWidth, player.videoHeight, true);
        } catch {
          if (!cancelled) setMessage("Camera scan unavailable. Choose a QR image.");
          return;
        }
      }
      if (!cancelled && !completed.current) timer = window.setTimeout(scanFrame, 180);
    }
    async function startCamera() {
      try {
        if (!player || !navigator.mediaDevices?.getUserMedia) throw new Error();
        const camera = await navigator.mediaDevices.getUserMedia({
          audio: false,
          video: { facingMode: { ideal: "environment" }, width: { ideal: 960 }, height: { ideal: 720 } },
        });
        if (cancelled) {
          camera.getTracks().forEach((track) => track.stop());
          return;
        }
        stream = camera;
        player.srcObject = stream;
        await player.play();
        if (cancelled) return;
        setCameraReady(true);
        setMessage("Point at a VmaX invite QR.");
        void scanFrame();
      } catch {
        stream?.getTracks().forEach((track) => track.stop());
        if (!cancelled) setMessage("Camera unavailable. Choose a QR image.");
      }
    }
    void startCamera();
    return () => {
      cancelled = true;
      active.current = false;
      window.clearTimeout(timer);
      stream?.getTracks().forEach((track) => track.stop());
      if (player) player.srcObject = null;
    };
  }, []);

  async function chooseImage(event: ChangeEvent<HTMLInputElement>) {
    const file = event.currentTarget.files?.[0];
    event.currentTarget.value = "";
    if (!file) return;
    try {
      const image = await createImageBitmap(file);
      try {
        await decode(image, image.width, image.height);
      } finally {
        image.close();
      }
    } catch {
      if (active.current) setMessage("Could not read that image.");
    }
  }

  return (
    <dialog ref={dialog} className="vmax-qr-scanner" aria-labelledby="vmax-qr-title" onCancel={(event) => {
      event.preventDefault();
      onClose();
    }}>
      <header>
        <h2 id="vmax-qr-title">Scan invite</h2>
        <button type="button" className="vmax-secondary" aria-label="Close QR scanner" onClick={onClose}><X size={18} /></button>
      </header>
      <video ref={video} muted playsInline hidden={!cameraReady} />
      <p role="status">{message}</p>
      <label className="vmax-secondary"><ImageUp size={17} /> Choose QR image
        <input type="file" accept="image/*" className="sr-only" onChange={(event) => void chooseImage(event)} />
      </label>
    </dialog>
  );
}
