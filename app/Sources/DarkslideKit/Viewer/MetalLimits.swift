import Metal

/// What a Metal device can allocate. Metal **aborts the process** (a descriptor
/// assertion, also in release builds) when asked for a texture larger than the device
/// limit, instead of returning nil, so every allocation of an image-sized texture or
/// buffer checks here first and takes its fallback when this says no.
public enum MetalLimits {
    /// The largest 2D texture edge: 16384 on Apple silicon and Mac2-family GPUs,
    /// 8192 on anything older.
    public static func maxTextureEdge(_ device: MTLDevice) -> Int {
        device.supportsFamily(.apple3) || device.supportsFamily(.mac2) ? 16384 : 8192
    }

    /// Whether a `width`×`height` 2D texture can be made on `device`.
    public static func canMakeTexture(_ device: MTLDevice, width: Int, height: Int) -> Bool {
        let edge = maxTextureEdge(device)
        return width > 0 && height > 0 && width <= edge && height <= edge
    }

    /// Whether a buffer of `length` bytes can be made on `device`.
    public static func canMakeBuffer(_ device: MTLDevice, length: Int) -> Bool {
        length > 0 && length <= device.maxBufferLength
    }
}
