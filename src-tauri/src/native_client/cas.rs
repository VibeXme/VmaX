use windows::Win32::Graphics::{
    Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob, ID3DInclude},
    Direct3D11::*,
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
};
use windows_core::{PCSTR, s};

const SOURCE: &str = include_str!("../../shaders/client_cas.hlsl");

pub(super) struct Shaders {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
}

impl Shaders {
    pub(super) fn new(device: &ID3D11Device) -> windows_core::Result<Self> {
        let vertex = compile(s!("fullscreen"), s!("vs_5_0"))?;
        let pixel = compile(s!("sharpen"), s!("ps_5_0"))?;
        let mut vs = None;
        let mut ps = None;
        unsafe {
            device.CreateVertexShader(bytecode(&vertex), None, Some(&mut vs))?;
            device.CreatePixelShader(bytecode(&pixel), None, Some(&mut ps))?;
        }
        Ok(Self {
            vertex: vs.unwrap(),
            pixel: ps.unwrap(),
        })
    }
}

pub(super) struct Pass {
    pub(super) input: ID3D11Texture2D,
    source: [Option<ID3D11ShaderResourceView>; 1],
    target: [Option<ID3D11RenderTargetView>; 1],
    viewport: [D3D11_VIEWPORT; 1],
    params: [Option<ID3D11Buffer>; 1],
    size: (u32, u32),
    strength: Option<u8>,
}

impl Pass {
    pub(super) fn new(
        device: &ID3D11Device,
        backbuffer: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> windows_core::Result<Self> {
        let mut input = None;
        let mut source = None;
        let mut target = None;
        let mut params = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut input),
            )?;
            let input = input.as_ref().unwrap();
            device.CreateShaderResourceView(input, None, Some(&mut source))?;
            device.CreateRenderTargetView(backbuffer, None, Some(&mut target))?;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut params),
            )?;
        }
        Ok(Self {
            input: input.unwrap(),
            source: [source],
            target: [target],
            viewport: [D3D11_VIEWPORT {
                Width: width as f32,
                Height: height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }],
            params: [params],
            size: (width, height),
            strength: None,
        })
    }

    pub(super) fn draw(&mut self, context: &ID3D11DeviceContext, shaders: &Shaders, strength: u8) {
        unsafe {
            if self.strength != Some(strength) {
                let peak = if strength == 0 {
                    0.0_f32
                } else {
                    -1.0_f32 / (8.0 - 3.0 * f32::from(strength.min(100)) / 100.0)
                };
                let params = [self.size.0 - 1, self.size.1 - 1, peak.to_bits(), 0];
                context.UpdateSubresource(
                    self.params[0].as_ref().unwrap(),
                    0,
                    None,
                    params.as_ptr().cast(),
                    0,
                    0,
                );
                self.strength = Some(strength);
            }
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.RSSetViewports(Some(&self.viewport));
            context.VSSetShader(&shaders.vertex, None);
            context.PSSetShader(&shaders.pixel, None);
            context.PSSetConstantBuffers(0, Some(&self.params));
            context.PSSetShaderResources(0, Some(&self.source));
            context.OMSetRenderTargets(Some(&self.target), None::<&ID3D11DepthStencilView>);
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        }
    }
}

fn compile(entry: PCSTR, profile: PCSTR) -> windows_core::Result<ID3DBlob> {
    let mut code = None;
    unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("client_cas.hlsl"),
            None,
            None::<&ID3DInclude>,
            entry,
            profile,
            0,
            0,
            &mut code,
            None,
        )?;
    }
    Ok(code.unwrap())
}

fn bytecode(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}
