import CoreVideo
import Metal
import MetalKit
import UIKit

/// Draws decoded frames with a plain textured quad, filling its bounds.
///
/// There is no render loop: the layer is redrawn only when a frame lands, which
/// keeps the GPU idle while the host shows a static screen. Frames are drawn on
/// a private queue straight from the decoder callback; if a new frame arrives
/// while one is still being drawn, only the newest is kept. The superview sizes
/// this view to the picture's aspect ratio, so stretching never distorts.
final class VideoView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }

    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    private let device: MTLDevice
    private let commandQueue: MTLCommandQueue
    private var pipeline: MTLRenderPipelineState?
    private var sampler: MTLSamplerState?
    private var textureCache: CVMTextureCache?
    private var pending: CVPixelBuffer?
    private var drawScheduled = false
    private let drawLock = NSLock()
    private let renderQueue = DispatchQueue(label: "dev.waystone.render", qos: .userInteractive)

    override init(frame: CGRect) {
        guard let device = MTLCreateSystemDefaultDevice(),
            let queue = device.makeCommandQueue()
        else {
            fatalError("Metal is required to render the remote screen")
        }
        self.device = device
        self.commandQueue = queue
        super.init(frame: frame)
        configure()
    }

    required init?(coder: NSCoder) {
        guard let device = MTLCreateSystemDefaultDevice(),
            let queue = device.makeCommandQueue()
        else { return nil }
        self.device = device
        self.commandQueue = queue
        super.init(coder: coder)
        configure()
    }

    private func configure() {
        metalLayer.device = device
        metalLayer.pixelFormat = .bgra8Unorm
        // Presentation without vsync keeps the glass-to-glass latency minimal.
        metalLayer.framebufferOnly = true
        metalLayer.maximumDrawableCount = 2
        metalLayer.isOpaque = true
        metalLayer.backgroundColor = UIColor.black.cgColor
        textureCache = CVMTextureCache(device: device)
        // One sampler for the lifetime of the view: creating it per frame is a
        // surprising amount of work at 60 Hz, especially on a simulator.
        let samplerDescriptor = MTLSamplerDescriptor()
        samplerDescriptor.minFilter = .linear
        samplerDescriptor.magFilter = .linear
        sampler = device.makeSamplerState(descriptor: samplerDescriptor)
        buildPipeline()
    }

    private func buildPipeline() {
        let source = """
        #include <metal_stdlib>
        using namespace metal;

        struct VertexOut {
            float4 position [[position]];
            float2 uv;
        };

        vertex VertexOut vertex_main(uint id [[vertex_id]]) {
            // A triangle strip covering the whole view.
            float2 positions[4] = { float2(-1, -1), float2(1, -1), float2(-1, 1), float2(1, 1) };
            float2 uvs[4] = { float2(0, 1), float2(1, 1), float2(0, 0), float2(1, 0) };
            VertexOut out;
            out.position = float4(positions[id], 0, 1);
            out.uv = uvs[id];
            return out;
        }

        fragment float4 fragment_main(VertexOut in [[stage_in]],
                                      texture2d<float> tex [[texture(0)]],
                                      sampler samp [[sampler(0)]]) {
            return tex.sample(samp, in.uv);
        }
        """
        guard let library = try? device.makeLibrary(source: source, options: nil),
            let vertexFunction = library.makeFunction(name: "vertex_main"),
            let fragmentFunction = library.makeFunction(name: "fragment_main")
        else {
            return
        }

        let descriptor = MTLRenderPipelineDescriptor()
        descriptor.vertexFunction = vertexFunction
        descriptor.fragmentFunction = fragmentFunction
        let attachment = descriptor.colorAttachments[0]!
        attachment.pixelFormat = metalLayer.pixelFormat
        pipeline = try? device.makeRenderPipelineState(descriptor: descriptor)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let scale = window?.screen.scale ?? UIScreen.main.scale
        metalLayer.contentsScale = scale
        metalLayer.drawableSize = CGSize(width: bounds.width * scale, height: bounds.height * scale)
    }

    /// Shows a decoded frame. Safe to call from any thread.
    func present(_ buffer: CVPixelBuffer) {
        drawLock.lock()
        pending = buffer
        let schedule = !drawScheduled
        drawScheduled = true
        drawLock.unlock()
        if schedule {
            renderQueue.async { [weak self] in self?.draw() }
        }
    }

    private func draw() {
        drawLock.lock()
        let buffer = pending
        pending = nil
        drawScheduled = false
        drawLock.unlock()
        guard let buffer, let pipeline, let sampler, let textureCache else { return }

        guard let (backing, texture) = textureCache.texture(for: buffer) else { return }
        guard metalLayer.drawableSize.width > 0, let drawable = metalLayer.nextDrawable() else { return }
        let descriptor = MTLRenderPassDescriptor()
        descriptor.colorAttachments[0].texture = drawable.texture
        descriptor.colorAttachments[0].loadAction = .clear
        descriptor.colorAttachments[0].storeAction = .store
        descriptor.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)

        guard let commandBuffer = commandQueue.makeCommandBuffer(),
            let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: descriptor)
        else { return }

        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(texture, index: 0)
        encoder.setFragmentSamplerState(sampler, index: 0)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        encoder.endEncoding()
        commandBuffer.present(drawable)
        commandBuffer.addCompletedHandler { _ in withExtendedLifetime(backing) {} }
        commandBuffer.commit()
    }
}

/// Caches `CVMetalTexture` objects so a 60 fps stream does not allocate each frame.
final class CVMTextureCache {
    private let cache: CVMetalTextureCache
    private let device: MTLDevice

    init?(device: MTLDevice) {
        self.device = device
        var cache: CVMetalTextureCache?
        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &cache) == kCVReturnSuccess,
            let cache
        else { return nil }
        self.cache = cache
    }

    /// The `CVMetalTexture` must outlive the GPU work that samples it, so
    /// both are returned and the caller keeps the first until completion.
    func texture(for buffer: CVPixelBuffer) -> (CVMetalTexture, MTLTexture)? {
        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        var metalTexture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            nil,
            cache,
            buffer,
            nil,
            .bgra8Unorm,
            width,
            height,
            0,
            &metalTexture)
        guard status == kCVReturnSuccess, let metalTexture,
            let texture = CVMetalTextureGetTexture(metalTexture)
        else { return nil }
        return (metalTexture, texture)
    }
}
