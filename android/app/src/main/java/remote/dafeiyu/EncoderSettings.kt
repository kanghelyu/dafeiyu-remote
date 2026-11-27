package remote.dafeiyu

/** Confirmed H.264 encoder settings. All dimensions are positive even pixels. */
internal data class EncoderSettings(
    val width: Int,
    val height: Int,
    val fps: Int,
    val bitrateKbps: Int
) {
    init {
        validate()
    }

    fun validate() {
        require(width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0) {
            "编码宽高必须为正偶数：${width}x${height}"
        }
        require(fps in 1..120) { "fps 必须在 1..=120 之间：$fps" }
        require(bitrateKbps in 100..100_000) {
            "码率必须在 100..=100000 kbps 之间：$bitrateKbps"
        }
    }

    companion object {
        const val DEFAULT_MAX_DIMENSION = 1920
        const val DEFAULT_FPS = 30
        const val DEFAULT_BITRATE_KBPS = 4000

        /** Fits the native source into an even bounding box without upscaling. */
        fun fit(
            sourceWidth: Int,
            sourceHeight: Int,
            maxDimension: Int = DEFAULT_MAX_DIMENSION,
            fps: Int = DEFAULT_FPS,
            bitrateKbps: Int = DEFAULT_BITRATE_KBPS
        ): EncoderSettings = fitWithin(
            sourceWidth, sourceHeight, maxDimension, maxDimension, fps, bitrateKbps
        )

        /** Fits the native source into a rectangular bounding box without upscaling. */
        fun fitWithin(
            sourceWidth: Int,
            sourceHeight: Int,
            maxWidth: Int,
            maxHeight: Int,
            fps: Int = DEFAULT_FPS,
            bitrateKbps: Int = DEFAULT_BITRATE_KBPS
        ): EncoderSettings {
            require(sourceWidth > 0 && sourceHeight > 0) {
                "采集尺寸必须为正数：${sourceWidth}x${sourceHeight}"
            }
            require(maxWidth >= 2 && maxHeight >= 2 && maxWidth % 2 == 0 && maxHeight % 2 == 0) {
                "最大编码尺寸必须为正偶数：${maxWidth}x${maxHeight}"
            }
            val nativeWidth = sourceWidth and 1.inv()
            val nativeHeight = sourceHeight and 1.inv()
            require(nativeWidth >= 2 && nativeHeight >= 2) {
                "采集尺寸过小，无法编码 NV12：${sourceWidth}x${sourceHeight}"
            }
            val width: Long
            val height: Long
            if (nativeWidth <= maxWidth && nativeHeight <= maxHeight) {
                width = nativeWidth.toLong()
                height = nativeHeight.toLong()
            } else if (maxWidth.toLong() * nativeHeight <= maxHeight.toLong() * nativeWidth) {
                width = maxWidth.toLong()
                height = maxWidth.toLong() * nativeHeight / nativeWidth
            } else {
                width = maxHeight.toLong() * nativeWidth / nativeHeight
                height = maxHeight.toLong()
            }
            return EncoderSettings(
                minOf(nativeWidth, (width and 1L.inv()).toInt()),
                minOf(nativeHeight, (height and 1L.inv()).toInt()),
                fps,
                bitrateKbps
            )
        }
    }
}
