/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://www.khronos.org/registry/webgl/specs/latest/1.0/webgl.idl

use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::EXTTextureFilterAnisotropicBinding::EXTTextureFilterAnisotropicConstants;
use crate::dom::bindings::codegen::Bindings::WebGLRenderingContextBinding::WebGLRenderingContextConstants as constants;
use crate::dom::bindings::codegen::Bindings::WebGLTextureBinding;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::{reflect_dom_object, DomObject};
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl_validations::types::TexImageTarget;
use crate::dom::webglobject::WebGLObject;
use crate::dom::webglrenderingcontext::WebGLRenderingContext;
use canvas_traits::webgl::{webgl_channel, TexDataType, TexFormat, WebGLResult, WebGLTextureId};
use canvas_traits::webgl::{DOMToTextureCommand, WebGLCommand, WebGLError};
use dom_struct::dom_struct;
use std::cell::Cell;
use std::cmp;

pub enum TexParameterValue {
    Float(f32),
    Int(i32),
}

const MAX_LEVEL_COUNT: usize = 31;
const MAX_FACE_COUNT: usize = 6;

jsmanaged_array!(MAX_LEVEL_COUNT * MAX_FACE_COUNT);

#[dom_struct]
pub struct WebGLTexture {
    webgl_object: WebGLObject,
    id: WebGLTextureId,
    /// The target to which this texture was bound the first time
    target: Cell<Option<u32>>,
    is_deleted: Cell<bool>,
    /// Stores information about mipmap levels and cubemap faces.
    #[ignore_malloc_size_of = "Arrays are cumbersome"]
    image_info_array: DomRefCell<[ImageInfo; MAX_LEVEL_COUNT * MAX_FACE_COUNT]>,
    /// Face count can only be 1 or 6
    face_count: Cell<u8>,
    base_mipmap_level: u32,
    // Store information for min and mag filters
    min_filter: Cell<u32>,
    mag_filter: Cell<u32>,
    /// True if this texture is used for the DOMToTexture feature.
    attached_to_dom: Cell<bool>,
}

impl WebGLTexture {
    fn new_inherited(context: &WebGLRenderingContext, id: WebGLTextureId) -> Self {
        Self {
            webgl_object: WebGLObject::new_inherited(context),
            id: id,
            target: Cell::new(None),
            is_deleted: Cell::new(false),
            face_count: Cell::new(0),
            base_mipmap_level: 0,
            min_filter: Cell::new(constants::NEAREST_MIPMAP_LINEAR),
            mag_filter: Cell::new(constants::LINEAR),
            image_info_array: DomRefCell::new([ImageInfo::new(); MAX_LEVEL_COUNT * MAX_FACE_COUNT]),
            attached_to_dom: Cell::new(false),
        }
    }

    pub fn maybe_new(context: &WebGLRenderingContext) -> Option<DomRoot<Self>> {
        let (sender, receiver) = webgl_channel().unwrap();
        context.send_command(WebGLCommand::CreateTexture(sender));
        receiver
            .recv()
            .unwrap()
            .map(|id| WebGLTexture::new(context, id))
    }

    pub fn new(context: &WebGLRenderingContext, id: WebGLTextureId) -> DomRoot<Self> {
        reflect_dom_object(
            Box::new(WebGLTexture::new_inherited(context, id)),
            &*context.global(),
            WebGLTextureBinding::Wrap,
        )
    }
}

impl WebGLTexture {
    pub fn id(&self) -> WebGLTextureId {
        self.id
    }

    // NB: Only valid texture targets come here
    pub fn bind(&self, target: u32) -> WebGLResult<()> {
        if self.is_deleted.get() {
            return Err(WebGLError::InvalidOperation);
        }

        if let Some(previous_target) = self.target.get() {
            if target != previous_target {
                return Err(WebGLError::InvalidOperation);
            }
        } else {
            // This is the first time binding
            let face_count = match target {
                constants::TEXTURE_2D => 1,
                constants::TEXTURE_CUBE_MAP => 6,
                _ => return Err(WebGLError::InvalidEnum),
            };
            self.face_count.set(face_count);
            self.target.set(Some(target));
        }

        self.upcast::<WebGLObject>()
            .context()
            .send_command(WebGLCommand::BindTexture(target, Some(self.id)));

        Ok(())
    }

    pub fn initialize(
        &self,
        target: TexImageTarget,
        width: u32,
        height: u32,
        depth: u32,
        internal_format: TexFormat,
        level: u32,
        data_type: Option<TexDataType>,
    ) -> WebGLResult<()> {
        let image_info = ImageInfo {
            width: width,
            height: height,
            depth: depth,
            internal_format: Some(internal_format),
            is_initialized: true,
            data_type: data_type,
        };

        let face_index = self.face_index_for_target(&target);
        self.set_image_infos_at_level_and_face(level, face_index, image_info);
        Ok(())
    }

    pub fn generate_mipmap(&self) -> WebGLResult<()> {
        let target = match self.target.get() {
            Some(target) => target,
            None => {
                error!("Cannot generate mipmap on texture that has no target!");
                return Err(WebGLError::InvalidOperation);
            },
        };

        let base_image_info = self.base_image_info();
        if !base_image_info.is_initialized() {
            return Err(WebGLError::InvalidOperation);
        }

        let is_cubic = target == constants::TEXTURE_CUBE_MAP;
        if is_cubic && !self.is_cube_complete() {
            return Err(WebGLError::InvalidOperation);
        }

        if !base_image_info.is_power_of_two() {
            return Err(WebGLError::InvalidOperation);
        }

        if base_image_info.is_compressed_format() {
            return Err(WebGLError::InvalidOperation);
        }

        self.upcast::<WebGLObject>()
            .context()
            .send_command(WebGLCommand::GenerateMipmap(target));

        if self.base_mipmap_level + base_image_info.get_max_mimap_levels() == 0 {
            return Err(WebGLError::InvalidOperation);
        }

        let last_level = self.base_mipmap_level + base_image_info.get_max_mimap_levels() - 1;
        self.populate_mip_chain(self.base_mipmap_level, last_level)
    }

    pub fn delete(&self, fallible: bool) {
        if !self.is_deleted.get() {
            self.is_deleted.set(true);
            let context = self.upcast::<WebGLObject>().context();
            // Notify WR to release the frame output when using DOMToTexture feature
            if self.attached_to_dom.get() {
                let _ = context
                    .webgl_sender()
                    .send_dom_to_texture(DOMToTextureCommand::Detach(self.id));
            }

            /*
            If a texture object is deleted while its image is attached to the currently
            bound framebuffer, then it is as if FramebufferTexture2D had been called, with
            a texture of 0, for each attachment point to which this image was attached
            in the currently bound framebuffer.
            - GLES 2.0, 4.4.3, "Attaching Texture Images to a Framebuffer"
             */
            let currently_bound_framebuffer =
                self.upcast::<WebGLObject>().context().bound_framebuffer();
            if let Some(fb) = currently_bound_framebuffer {
                fb.detach_texture(self);
            }

            let cmd = WebGLCommand::DeleteTexture(self.id);
            if fallible {
                context.send_command_ignored(cmd);
            } else {
                context.send_command(cmd);
            }
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.is_deleted.get()
    }

    pub fn target(&self) -> Option<u32> {
        self.target.get()
    }

    /// We have to follow the conversion rules for GLES 2.0. See:
    ///   https://www.khronos.org/webgl/public-mailing-list/archives/1008/msg00014.html
    ///
    pub fn tex_parameter(&self, param: u32, value: TexParameterValue) -> WebGLResult<()> {
        let target = self.target().unwrap();

        let (int_value, float_value) = match value {
            TexParameterValue::Int(int_value) => (int_value, int_value as f32),
            TexParameterValue::Float(float_value) => (float_value as i32, float_value),
        };

        let update_filter = |filter: &Cell<u32>| {
            if filter.get() == int_value as u32 {
                return Ok(());
            }
            filter.set(int_value as u32);
            self.upcast::<WebGLObject>()
                .context()
                .send_command(WebGLCommand::TexParameteri(target, param, int_value));
            Ok(())
        };
        match param {
            constants::TEXTURE_MIN_FILTER => match int_value as u32 {
                constants::NEAREST |
                constants::LINEAR |
                constants::NEAREST_MIPMAP_NEAREST |
                constants::LINEAR_MIPMAP_NEAREST |
                constants::NEAREST_MIPMAP_LINEAR |
                constants::LINEAR_MIPMAP_LINEAR => update_filter(&self.min_filter),
                _ => Err(WebGLError::InvalidEnum),
            },
            constants::TEXTURE_MAG_FILTER => match int_value as u32 {
                constants::NEAREST | constants::LINEAR => update_filter(&self.mag_filter),
                _ => return Err(WebGLError::InvalidEnum),
            },
            constants::TEXTURE_WRAP_S | constants::TEXTURE_WRAP_T => match int_value as u32 {
                constants::CLAMP_TO_EDGE | constants::MIRRORED_REPEAT | constants::REPEAT => {
                    self.upcast::<WebGLObject>()
                        .context()
                        .send_command(WebGLCommand::TexParameteri(target, param, int_value));
                    Ok(())
                },
                _ => Err(WebGLError::InvalidEnum),
            },
            EXTTextureFilterAnisotropicConstants::TEXTURE_MAX_ANISOTROPY_EXT => {
                // NaN is not less than 1., what a time to be alive.
                if !(float_value >= 1.) {
                    return Err(WebGLError::InvalidValue);
                }
                self.upcast::<WebGLObject>()
                    .context()
                    .send_command(WebGLCommand::TexParameterf(target, param, float_value));
                Ok(())
            },
            _ => Err(WebGLError::InvalidEnum),
        }
    }

    pub fn min_filter(&self) -> u32 {
        self.min_filter.get()
    }

    pub fn mag_filter(&self) -> u32 {
        self.mag_filter.get()
    }

    pub fn is_using_linear_filtering(&self) -> bool {
        let filters = [self.min_filter.get(), self.mag_filter.get()];
        filters.iter().any(|filter| match *filter {
            constants::LINEAR |
            constants::NEAREST_MIPMAP_LINEAR |
            constants::LINEAR_MIPMAP_NEAREST |
            constants::LINEAR_MIPMAP_LINEAR => true,
            _ => false,
        })
    }

    pub fn populate_mip_chain(&self, first_level: u32, last_level: u32) -> WebGLResult<()> {
        let base_image_info = self.image_info_at_face(0, first_level);
        if !base_image_info.is_initialized() {
            return Err(WebGLError::InvalidOperation);
        }

        let mut ref_width = base_image_info.width;
        let mut ref_height = base_image_info.height;

        if ref_width == 0 || ref_height == 0 {
            return Err(WebGLError::InvalidOperation);
        }

        for level in (first_level + 1)..last_level {
            if ref_width == 1 && ref_height == 1 {
                break;
            }

            ref_width = cmp::max(1, ref_width / 2);
            ref_height = cmp::max(1, ref_height / 2);

            let image_info = ImageInfo {
                width: ref_width,
                height: ref_height,
                depth: 0,
                internal_format: base_image_info.internal_format,
                is_initialized: base_image_info.is_initialized(),
                data_type: base_image_info.data_type,
            };

            self.set_image_infos_at_level(level, image_info);
        }
        Ok(())
    }

    fn is_cube_complete(&self) -> bool {
        debug_assert_eq!(self.face_count.get(), 6);

        let image_info = self.base_image_info();
        if !image_info.is_defined() {
            return false;
        }

        let ref_width = image_info.width;
        let ref_format = image_info.internal_format;

        for face in 0..self.face_count.get() {
            let current_image_info = self.image_info_at_face(face, self.base_mipmap_level);
            if !current_image_info.is_defined() {
                return false;
            }

            // Compares height with width to enforce square dimensions
            if current_image_info.internal_format != ref_format ||
                current_image_info.width != ref_width ||
                current_image_info.height != ref_width
            {
                return false;
            }
        }

        true
    }

    fn face_index_for_target(&self, target: &TexImageTarget) -> u8 {
        match *target {
            TexImageTarget::Texture2D => 0,
            TexImageTarget::CubeMapPositiveX => 0,
            TexImageTarget::CubeMapNegativeX => 1,
            TexImageTarget::CubeMapPositiveY => 2,
            TexImageTarget::CubeMapNegativeY => 3,
            TexImageTarget::CubeMapPositiveZ => 4,
            TexImageTarget::CubeMapNegativeZ => 5,
        }
    }

    pub fn image_info_for_target(&self, target: &TexImageTarget, level: u32) -> ImageInfo {
        let face_index = self.face_index_for_target(&target);
        self.image_info_at_face(face_index, level)
    }

    pub fn image_info_at_face(&self, face: u8, level: u32) -> ImageInfo {
        let pos = (level * self.face_count.get() as u32) + face as u32;
        self.image_info_array.borrow()[pos as usize]
    }

    fn set_image_infos_at_level(&self, level: u32, image_info: ImageInfo) {
        for face in 0..self.face_count.get() {
            self.set_image_infos_at_level_and_face(level, face, image_info);
        }
    }

    fn set_image_infos_at_level_and_face(&self, level: u32, face: u8, image_info: ImageInfo) {
        debug_assert!(face < self.face_count.get());
        let pos = (level * self.face_count.get() as u32) + face as u32;
        self.image_info_array.borrow_mut()[pos as usize] = image_info;
    }

    fn base_image_info(&self) -> ImageInfo {
        assert!((self.base_mipmap_level as usize) < MAX_LEVEL_COUNT);

        self.image_info_at_face(0, self.base_mipmap_level)
    }

    pub fn set_attached_to_dom(&self) {
        self.attached_to_dom.set(true);
    }
}

impl Drop for WebGLTexture {
    fn drop(&mut self) {
        self.delete(true);
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub struct ImageInfo {
    width: u32,
    height: u32,
    depth: u32,
    internal_format: Option<TexFormat>,
    is_initialized: bool,
    data_type: Option<TexDataType>,
}

impl ImageInfo {
    fn new() -> ImageInfo {
        ImageInfo {
            width: 0,
            height: 0,
            depth: 0,
            internal_format: None,
            is_initialized: false,
            data_type: None,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn internal_format(&self) -> Option<TexFormat> {
        self.internal_format
    }

    pub fn data_type(&self) -> Option<TexDataType> {
        self.data_type
    }

    fn is_power_of_two(&self) -> bool {
        self.width.is_power_of_two() &&
            self.height.is_power_of_two() &&
            self.depth.is_power_of_two()
    }

    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }

    fn is_defined(&self) -> bool {
        self.internal_format.is_some()
    }

    fn get_max_mimap_levels(&self) -> u32 {
        let largest = cmp::max(cmp::max(self.width, self.height), self.depth);
        if largest == 0 {
            return 0;
        }
        // FloorLog2(largest) + 1
        (largest as f64).log2() as u32 + 1
    }

    fn is_compressed_format(&self) -> bool {
        match self.internal_format {
            Some(format) => format.is_compressed(),
            None => false,
        }
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
pub enum TexCompressionValidation {
    None,
    S3TC,
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
pub struct TexCompression {
    pub format: TexFormat,
    pub bytes_per_block: u8,
    pub block_width: u8,
    pub block_height: u8,
    pub validation: TexCompressionValidation,
}
