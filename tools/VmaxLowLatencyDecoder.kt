package com.vmax.app

import android.app.Activity
import android.graphics.Color
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.view.Gravity
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.webkit.WebView
import android.widget.FrameLayout

object VmaxLowLatencyDecoder {
    private data class Format(val mime: String, val width: Int, val height: Int, val profile: Int)

    private val ui = Handler(Looper.getMainLooper())
    private val outputHandler by lazy {
        Handler(HandlerThread("vmax-mediacodec-output").apply { start() }.looper)
    }
    private val outputPoll = Runnable { pollOutput() }
    private var view: SurfaceView? = null
    private var webView: WebView? = null
    private var surface: Surface? = null
    private var decoder: MediaCodec? = null
    private var format: Format? = null
    private var waitingForKeyframe = true
    private var failures = 0
    private var outputDeadline = 0L
    private var pollScheduled = false
    private val hardwareDecoders by lazy {
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter {
            !it.isEncoder && if (Build.VERSION.SDK_INT >= 29) {
                it.isHardwareAccelerated
            } else {
                val name = it.name.lowercase()
                !name.startsWith("omx.google.") && !name.startsWith("c2.android.") && !name.startsWith("c2.google.")
            }
        }
    }

    @JvmStatic private external fun nativeSetDecoderActive(active: Boolean)
    @JvmStatic private external fun nativeSetDecoderCapabilities(codecs: Int)
    @JvmStatic private external fun nativeNotifyDecoderReset()
    @JvmStatic private external fun nativeReportVideoSize(width: Int, height: Int)
    @JvmStatic private external fun nativeNotifyFrameRendered()

    fun attach(activity: Activity, web: WebView) {
        web.setBackgroundColor(Color.TRANSPARENT)
        web.post {
            if (webView === web || activity.isDestroyed) return@post
            val root = activity.findViewById<FrameLayout>(android.R.id.content) ?: return@post
            view?.let { current -> (current.parent as? FrameLayout)?.removeView(current) }
            val target = SurfaceView(activity)
            view = target
            webView = web
            root.setBackgroundColor(Color.BLACK)
            target.importantForAccessibility = SurfaceView.IMPORTANT_FOR_ACCESSIBILITY_NO
            target.holder.addCallback(object : SurfaceHolder.Callback {
                override fun surfaceCreated(holder: SurfaceHolder) {
                    synchronized(this@VmaxLowLatencyDecoder) {
                        if (view !== target) return
                        surface = holder.surface
                        failures = 0
                        nativeSetDecoderCapabilities(runCatching { decoderCapabilities() }.getOrDefault(0))
                        nativeSetDecoderActive(true)
                    }
                }

                override fun surfaceChanged(holder: SurfaceHolder, pixelFormat: Int, width: Int, height: Int) {}

                override fun surfaceDestroyed(holder: SurfaceHolder) {
                    synchronized(this@VmaxLowLatencyDecoder) {
                        if (view !== target) return
                        nativeSetDecoderActive(false)
                        release()
                        surface = null
                    }
                }
            })
            target.addOnAttachStateChangeListener(object : android.view.View.OnAttachStateChangeListener {
                override fun onViewAttachedToWindow(v: android.view.View) {}
                override fun onViewDetachedFromWindow(v: android.view.View) {
                    if (view === v) {
                        view = null
                        webView = null
                    }
                }
            })
            root.addView(target, 0, FrameLayout.LayoutParams(1, 1))
        }
    }

    @JvmStatic
    fun layoutSurface(x: Int, y: Int, width: Int, height: Int, visible: Boolean) {
        ui.post {
            val target = view ?: return@post
            val web = webView ?: return@post
            val root = target.parent as? FrameLayout ?: return@post
            if (!visible) {
                target.layoutParams = FrameLayout.LayoutParams(1, 1)
                return@post
            }
            val webOrigin = IntArray(2)
            val rootOrigin = IntArray(2)
            web.getLocationInWindow(webOrigin)
            root.getLocationInWindow(rootOrigin)
            target.layoutParams = FrameLayout.LayoutParams(
                width.coerceAtLeast(1), height.coerceAtLeast(1), Gravity.TOP or Gravity.LEFT,
            ).apply {
                leftMargin = x + webOrigin[0] - rootOrigin[0]
                topMargin = y + webOrigin[1] - rootOrigin[1]
            }
        }
    }

    @JvmStatic
    @Synchronized
    fun resetSession() {
        release()
        failures = 0
        layoutSurface(0, 0, 1, 1, false)
    }

    @JvmStatic
    @Synchronized
    fun onNativeVideoFrame(bytes: ByteArray, timestampUs: Long, codec: Int, width: Int, height: Int, keyframe: Boolean) {
        val target = surface ?: return
        if (failures >= 3 || width !in 16..8192 || height !in 16..8192) return
        val stream = when (codec) {
            1 -> Format(MediaFormat.MIMETYPE_VIDEO_HEVC, width, height, MediaCodecInfo.CodecProfileLevel.HEVCProfileMain)
            2 -> Format(MediaFormat.MIMETYPE_VIDEO_AVC, width, height, 0)
            3 -> Format(MediaFormat.MIMETYPE_VIDEO_HEVC, width, height, MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10)
            5 -> Format(MediaFormat.MIMETYPE_VIDEO_AV1, width, height, 0)
            else -> return
        }
        try {
            if (stream != format) {
                release()
                if (!keyframe) return
                start(stream, target)
            }
            if (waitingForKeyframe && !keyframe) return
            val active = decoder ?: return
            drain(active)
            val index = active.dequeueInputBuffer(2_000)
            if (index < 0) {
                active.flush()
                waitingForKeyframe = true
                nativeNotifyDecoderReset()
                return
            }
            val input = checkNotNull(active.getInputBuffer(index))
            input.clear()
            input.put(bytes)
            active.queueInputBuffer(index, 0, bytes.size, timestampUs, if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
            waitingForKeyframe = false
            drain(active)
            outputDeadline = SystemClock.uptimeMillis() + 100
            scheduleOutputPoll()
        } catch (error: RuntimeException) {
            recover(error)
        }
    }

    private fun start(stream: Format, target: Surface) {
        val settings = MediaFormat.createVideoFormat(stream.mime, stream.width, stream.height)
        if (stream.profile != 0) settings.setInteger(MediaFormat.KEY_PROFILE, stream.profile)
        if (Build.VERSION.SDK_INT >= 30) settings.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        val codec = hardwareDecoders.firstOrNull { info ->
            info.supportedTypes.any { it.equals(stream.mime, ignoreCase = true) } &&
                info.getCapabilitiesForType(stream.mime).isFormatSupported(settings)
        } ?: throw IllegalStateException("Hardware decoder unavailable")
        val active = MediaCodec.createByCodecName(codec.name)
        decoder = active
        active.configure(settings, target, null, 0)
        active.setOnFrameRenderedListener({ source, _, _ ->
            synchronized(this) {
                if (source === decoder) nativeNotifyFrameRendered()
            }
        }, ui)
        active.start()
        format = stream
        nativeReportVideoSize(stream.width, stream.height)
    }

    private fun decoderCapabilities(): Int {
        var codecs = 0
        for (info in hardwareDecoders) {
            for (mime in info.supportedTypes) {
                when (mime.lowercase()) {
                    MediaFormat.MIMETYPE_VIDEO_AVC -> codecs = codecs or (1 shl 2)
                    MediaFormat.MIMETYPE_VIDEO_AV1 -> codecs = codecs or (1 shl 5)
                    MediaFormat.MIMETYPE_VIDEO_HEVC -> {
                        codecs = codecs or (1 shl 1)
                        val profiles = info.getCapabilitiesForType(mime).profileLevels
                        if (profiles.any { it.profile == MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10 }) {
                            codecs = codecs or (1 shl 3)
                        }
                    }
                }
            }
        }
        return codecs
    }

    @Synchronized
    private fun pollOutput() {
        pollScheduled = false
        val active = decoder ?: return
        if (waitingForKeyframe) return
        try {
            drain(active)
            if (SystemClock.uptimeMillis() < outputDeadline) scheduleOutputPoll()
        } catch (error: RuntimeException) {
            recover(error)
        }
    }

    private fun scheduleOutputPoll() {
        if (!pollScheduled) pollScheduled = outputHandler.postDelayed(outputPoll, 3)
    }

    private fun drain(active: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        var newest = -1
        for (attempt in 0 until 8) {
            val index = active.dequeueOutputBuffer(info, 0)
            when {
                index >= 0 -> {
                    if (newest >= 0) active.releaseOutputBuffer(newest, false)
                    newest = index
                }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> Unit
                else -> break
            }
        }
        if (newest >= 0) active.releaseOutputBuffer(newest, true)
    }

    private fun recover(error: RuntimeException) {
        android.util.Log.w("VmaX", "MediaCodec stream failed", error)
        failures++
        release()
        nativeNotifyDecoderReset()
    }

    private fun release() {
        val active = decoder
        decoder = null
        format = null
        waitingForKeyframe = true
        try {
            active?.release()
        } catch (error: IllegalStateException) {
            android.util.Log.w("VmaX", "MediaCodec release failed", error)
        }
    }
}
