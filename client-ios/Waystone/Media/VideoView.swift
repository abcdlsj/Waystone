import CoreVideo
import Metal
import MetalKit
import UIKit

/// Draws decoded frames with a plain textured quad.
///
/// A full `MTKView` render loop is unnecessary here: frames arrive at 45–60 Hz and
/// the layer is only redrawn when a new frame lands, which keeps the GPU idle while
/// the host shows a static screen.
final class VideoView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }

    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    private let device: MTLDevice
    private let commandQueue: MTLCommandQueue
    private var pipeline: MTLRenderPipelineState?
    private var sampler: MTLSamplerState?
    private var textureCache: CVMTextureCache?
    private var pending: CVPixelBuffer?
    private let drawLock = NSLock()

    /// Size of the remote screen, used to letterbox correctly.
    private(set) var videoSize: CGSize = .zero

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

    /// Shows a new decoded frame, scaled to fill the layer.
    func present(_ buffer: CVPixelBuffer, videoSize: CGSize) {
        drawLock.lock()
        pending = buffer
        drawLock.unlock()
        if videoSize != .zero {
            self.videoSize = videoSize
        }
        draw()
    }

    /// Clears the view; used when the stream stalls so the user never sees a
    /// half-updated frame.
    func clear() {
        metalLayer.backgroundColor = UIColor.black.cgColor
    }

    private func draw() {
        drawLock.lock()
        let buffer = pending
        pending = nil
        drawLock.unlock()
        guard let buffer, let pipeline, let sampler, let textureCache else { return }

        guard let texture = textureCache.texture(for: buffer) else { return }
        guard let drawable = metalLayer.nextDrawable() else { return }
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

    func texture(for buffer: CVPixelBuffer) -> MTLTexture? {
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
        guard status == kCVReturnSuccess, let metalTexture else { return nil }
        return CVMetalTextureGetTexture(metalTexture)
    }
}
