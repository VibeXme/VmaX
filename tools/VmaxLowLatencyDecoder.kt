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
    private var view: SurfaceView? = null
    private var webView: WebView? = null
    private var surface: Surface? = null
    private var decoder: MediaCodec? = null
    private var format: Format? = null
    private var waitingForKeyframe = true
    private var failures = 0
    private data class AccessUnit(val bytes: ByteArray, val pts: Long, val format: Format, val keyframe: Boolean)
    private data class Output(val index: Int, val pts: Long)

    private val inputs = ArrayDeque<Int>()
    private var pending: AccessUnit? = null
    private var inputPosted = false
    private var pendingReferenceGap = false
    private var output: Output? = null
    private val submitPending = Runnable {
        synchronized(this) {
            inputPosted = false
            try {
                submit()
            } catch (error: RuntimeException) {
                recover(error)
            }
        }
    }
    private val renderPending = Runnable {
        synchronized(this) {
            val frame = output
            output = null
            val active = decoder
            if (frame != null && active != null) {
                try {
                    remember(released, frame.pts)
                    active.releaseOutputBuffer(frame.index, true)
                } catch (error: RuntimeException) {
                    recover(error)
                }
            }
        }
    }
    private val submitted = LinkedHashMap<Long, Long>()
    private val released = LinkedHashMap<Long, Long>()
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
    @JvmStatic private external fun nativeReportDecoded(decodeUs: Int)
    @JvmStatic private external fun nativeNotifyFrameRendered(renderUs: Int)

    private fun attach(activity: Activity, web: WebView) {
        if (webView === web || activity.isDestroyed || !web.isAttachedToWindow) return
        val root = activity.findViewById<FrameLayout>(android.R.id.content) ?: return
        view?.let { current -> (current.parent as? FrameLayout)?.removeView(current) }
        val target = SurfaceView(activity)
        view = target
        webView = web
        root.setBackgroundColor(Color.BLACK)
        target.importantForAccessibility = SurfaceView.IMPORTANT_FOR_ACCESSIBILITY_NO
        target.holder.addCallback(object : SurfaceHolder.Callback {
            private var registered = false

            override fun surfaceCreated(holder: SurfaceHolder) {
                synchronized(this@VmaxLowLatencyDecoder) {
                    if (view !== target) return
                    surface = holder.surface
                    failures = 0
                    val codecs = runCatching { decoderCapabilities() }.getOrDefault(0)
                    if (codecs == 0) return
                    try {
                        nativeSetDecoderCapabilities(codecs)
                        nativeSetDecoderActive(true)
                        registered = true
                    } catch (error: LinkageError) {
                        surface = null
                        android.util.Log.w("VmaX", "Native video callbacks unavailable", error)
                    }
                }
            }

            override fun surfaceChanged(holder: SurfaceHolder, pixelFormat: Int, width: Int, height: Int) {}

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                synchronized(this@VmaxLowLatencyDecoder) {
                    if (view !== target) return
                    if (registered) nativeSetDecoderActive(false)
                    registered = false
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
        web.setBackgroundColor(Color.TRANSPARENT)
    }

    @JvmStatic
    fun layoutSurface(activity: Activity, web: WebView, x: Int, y: Int, width: Int, height: Int, visible: Boolean) {
        ui.post {
            if (visible && webView !== web) {
                try {
                    attach(activity, web)
                } catch (error: RuntimeException) {
                    android.util.Log.w("VmaX", "Could not attach the native video surface", error)
                }
            }
            val target = view ?: return@post
            if (webView !== web) return@post
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
        ui.post { view?.layoutParams = FrameLayout.LayoutParams(1, 1) }
    }

    @JvmStatic
    @Synchronized
    fun onNativeVideoFrame(bytes: ByteArray, timestampUs: Long, codec: Int, width: Int, height: Int, keyframe: Boolean) {
        if (surface == null || failures >= 3 || width !in 16..8192 || height !in 16..8192) return
        val stream = when (codec) {
            1 -> Format(MediaFormat.MIMETYPE_VIDEO_HEVC, width, height, MediaCodecInfo.CodecProfileLevel.HEVCProfileMain)
            2 -> Format(MediaFormat.MIMETYPE_VIDEO_AVC, width, height, 0)
            3 -> Format(MediaFormat.MIMETYPE_VIDEO_HEVC, width, height, MediaCodecInfo.CodecProfileLevel.HEVCProfileMain10)
            5 -> Format(MediaFormat.MIMETYPE_VIDEO_AV1, width, height, 0)
            else -> return
        }
        if (!keyframe) {
            if (pending != null) {
                // The new delta references an access unit we couldn't submit.
                // Keep a pending IDR if present and wait for clean recovery.
                if (waitingForKeyframe && pendingReferenceGap) return
                if (pending?.keyframe == true) pendingReferenceGap = true else pending = null
                waitingForKeyframe = true
                nativeNotifyDecoderReset()
                return
            }
            if (waitingForKeyframe || stream != format) return
        }
        pending = AccessUnit(bytes, timestampUs, stream, keyframe)
        pendingReferenceGap = false
        if (!inputPosted) inputPosted = outputHandler.post(submitPending)
    }

    private fun submit() {
        val frame = pending ?: return
        val target = surface ?: return
        if (frame.format != format) {
            if (!frame.keyframe) return
            val gap = pendingReferenceGap
            release()
            pending = frame
            pendingReferenceGap = gap
            start(frame.format, target)
        }
        val active = decoder ?: return
        val index = inputs.removeFirstOrNull() ?: return
        pending = null
        val input = checkNotNull(active.getInputBuffer(index))
        input.clear()
        input.put(frame.bytes)
        remember(submitted, frame.pts)
        active.queueInputBuffer(
            index, 0, frame.bytes.size, frame.pts,
            if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0,
        )
        waitingForKeyframe = pendingReferenceGap
        pendingReferenceGap = false
    }

    private fun start(stream: Format, target: Surface) {
        fun baseFormat() = MediaFormat.createVideoFormat(stream.mime, stream.width, stream.height).apply {
            if (stream.profile != 0) setInteger(MediaFormat.KEY_PROFILE, stream.profile)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        val settings = baseFormat()
        val codec = hardwareDecoders.firstOrNull { info ->
            info.supportedTypes.any { it.equals(stream.mime, ignoreCase = true) } &&
                info.getCapabilitiesForType(stream.mime).isFormatSupported(settings)
        } ?: throw IllegalStateException("Hardware decoder unavailable")
        settings.setFloat(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toFloat())
        fun configure(format: MediaFormat): MediaCodec {
            val active = MediaCodec.createByCodecName(codec.name)
            decoder = active
            active.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(source: MediaCodec, index: Int) {
                    synchronized(this@VmaxLowLatencyDecoder) {
                        if (source !== decoder) return
                        inputs.addLast(index)
                        try {
                            submit()
                        } catch (error: RuntimeException) {
                            recover(error)
                        }
                    }
                }

                override fun onOutputBufferAvailable(source: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    synchronized(this@VmaxLowLatencyDecoder) {
                        if (source !== decoder) return
                        try {
                            if ((info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0) {
                                source.releaseOutputBuffer(index, false)
                                return
                            }
                            val pts = info.presentationTimeUs
                            nativeReportDecoded(elapsedUs(submitted.remove(pts), System.nanoTime()))
                            val previous = output
                            if (previous != null) source.releaseOutputBuffer(previous.index, false)
                            output = Output(index, pts)
                            if (previous == null) outputHandler.post(renderPending)
                        } catch (error: RuntimeException) {
                            recover(error)
                        }
                    }
                }

                override fun onOutputFormatChanged(source: MediaCodec, format: MediaFormat) = Unit

                override fun onError(source: MediaCodec, error: MediaCodec.CodecException) {
                    synchronized(this@VmaxLowLatencyDecoder) {
                        if (source === decoder) recover(error)
                    }
                }
            }, outputHandler)
            active.configure(format, target, null, 0)
            return active
        }
        val active = try {
            configure(settings)
        } catch (error: RuntimeException) {
            // Some vendor codecs reject the operating-rate hint.
            val frame = pending
            val gap = pendingReferenceGap
            release()
            pending = frame
            pendingReferenceGap = gap
            configure(baseFormat())
        }
        active.setOnFrameRenderedListener({ source, pts, renderTime ->
            synchronized(this) {
                if (source === decoder) {
                    nativeNotifyFrameRendered(elapsedUs(released.remove(pts), renderTime))
                }
            }
        }, outputHandler)
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

    private fun remember(timestamps: LinkedHashMap<Long, Long>, pts: Long) {
        if (timestamps.size >= 32) timestamps.remove(timestamps.keys.first())
        timestamps[pts] = System.nanoTime()
    }

    private fun elapsedUs(start: Long?, end: Long): Int {
        return if (start == null) 0 else ((end - start) / 1_000L).coerceIn(0, 250_000).toInt()
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
        pending = null
        pendingReferenceGap = false
        inputs.clear()
        output = null
        inputPosted = false
        outputHandler.removeCallbacks(submitPending)
        outputHandler.removeCallbacks(renderPending)
        submitted.clear()
        released.clear()
        try {
            active?.release()
        } catch (error: IllegalStateException) {
            android.util.Log.w("VmaX", "MediaCodec release failed", error)
        }
    }
}
