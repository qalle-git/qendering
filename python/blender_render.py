"""Qendering in-Blender render worker.

Runs inside Blender:

    blender -b -P python/blender_render.py -- --worker

Persistent worker: reads one JSON item per line from stdin, renders it, and
writes a ``RESULT:{json}`` line to stdout. Supports two item kinds:

    {"type":"item",   "ydd_path":..., "dds_files":[...], "output_path":..., "category":...}
    {"type":"object", "ydr_path":..., "output_path":...}

An optional ``CONFIG:{json}`` line (render_size / taa_samples) may precede the
items. This is a cleaned port of the proven clothing_tool render script; the
rendering logic is kept faithful because it is known to work.

Requires Blender 4.x (Eevee Next) with the Sollumz add-on (+ PyMateria for
binary .ydd/.ydr import).
"""

import json
import math
import os
import shutil
import sys
import tempfile
import traceback

import bpy                       # type: ignore[import-untyped]
import addon_utils               # type: ignore[import-untyped]
from mathutils import Vector     # type: ignore[import-untyped]

# ---------------------------------------------------------------------------
# Enable Sollumz
# ---------------------------------------------------------------------------

_SOLLUMZ_MODULES = [
    "bl_ext.blender_org.sollumz_dev",
    "bl_ext.blender_org.sollumz",
    "SollumzPlugin",
]

for _mod_name in _SOLLUMZ_MODULES:
    try:
        addon_utils.enable(_mod_name)
        print(f"Enabled Sollumz addon: {_mod_name}")
        break
    except Exception:
        continue
else:
    print("WARNING: Could not enable Sollumz addon — imports may fail",
          file=sys.stderr)

# ---------------------------------------------------------------------------
# Tunables (overridable via CONFIG line)
# ---------------------------------------------------------------------------

RENDER_SIZE = 1024
TAA_SAMPLES = 1
CAMERA_ELEVATION_DEG = 10
PADDING_FACTOR = 1.15

OBJECT_AZIMUTH_DEG = 45.0
OBJECT_ELEVATION_DEG = 25.0
OBJECT_PADDING_FACTOR = 1.25

# Still image format: WEBP / PNG (both keep alpha) or JPEG (opaque, white bg).
STILL_FORMAT = "WEBP"


# ---------------------------------------------------------------------------
# Scene setup
# ---------------------------------------------------------------------------

def clear_scene() -> None:
    bpy.ops.object.select_all(action="DESELECT")
    for obj in list(bpy.data.objects):
        bpy.data.objects.remove(obj, do_unlink=True)
    for mesh in list(bpy.data.meshes):
        if mesh.users == 0:
            bpy.data.meshes.remove(mesh)
    for mat in list(bpy.data.materials):
        if mat.users == 0:
            bpy.data.materials.remove(mat)
    for img in list(bpy.data.images):
        if img.users == 0:
            bpy.data.images.remove(img)
    for coll in list(bpy.data.collections):
        bpy.data.collections.remove(coll)


def _setup_world_ambient(strength=0.28) -> None:
    """Dim neutral world so shadowed sides read instead of going pure black.

    Only contributes lighting; the film stays transparent so it never tints the
    background (a JPEG's white backdrop is composited separately).
    """
    scene = bpy.context.scene
    world = scene.world or bpy.data.worlds.new("QWorld")
    scene.world = world
    world.use_nodes = True
    bg = world.node_tree.nodes.get("Background")
    if bg:
        bg.inputs[0].default_value = (0.55, 0.57, 0.6, 1.0)
        bg.inputs[1].default_value = strength


def _setup_white_backdrop(enable: bool) -> None:
    """Composite the transparent render over solid white (for JPEG output).

    Keeps `film_transparent` on so scene lighting is identical across formats;
    only the saved pixels get a white background instead of black.
    """
    scene = bpy.context.scene
    scene.use_nodes = enable
    if not enable:
        return
    tree = scene.node_tree
    tree.nodes.clear()
    layers = tree.nodes.new("CompositorNodeRLayers")
    white = tree.nodes.new("CompositorNodeRGB")
    white.outputs[0].default_value = (1.0, 1.0, 1.0, 1.0)
    over = tree.nodes.new("CompositorNodeAlphaOver")
    composite = tree.nodes.new("CompositorNodeComposite")
    tree.links.new(white.outputs[0], over.inputs[1])          # background
    tree.links.new(layers.outputs["Image"], over.inputs[2])   # render on top
    tree.links.new(over.outputs[0], composite.inputs[0])


def _eevee_engine_id() -> str:
    """Return the correct Eevee engine identifier for this Blender version.

    Blender 4.2-4.4 expose the new Eevee as ``BLENDER_EEVEE_NEXT``; Blender 5.0
    dropped legacy Eevee and renamed it back to ``BLENDER_EEVEE``. Probe the
    actual enum so we pick whichever exists instead of guessing by version.
    """
    try:
        items = bpy.types.RenderSettings.bl_rna.properties["engine"].enum_items
        ids = {i.identifier for i in items}
    except Exception:
        ids = set()
    if "BLENDER_EEVEE_NEXT" in ids:
        return "BLENDER_EEVEE_NEXT"
    return "BLENDER_EEVEE"


def setup_render_settings() -> None:
    scene = bpy.context.scene
    scene.render.engine = _eevee_engine_id()
    scene.render.resolution_x = RENDER_SIZE
    scene.render.resolution_y = RENDER_SIZE
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = True
    scene.render.image_settings.file_format = STILL_FORMAT
    scene.render.image_settings.quality = 90
    # JPEG has no alpha: flatten the transparent render onto white in the
    # compositor. WEBP/PNG keep the transparent film as-is.
    if STILL_FORMAT == "JPEG":
        scene.render.image_settings.color_mode = "RGB"
        _setup_white_backdrop(True)
    else:
        scene.render.image_settings.color_mode = "RGBA"
        _setup_white_backdrop(False)
    scene.render.use_simplify = True
    scene.render.simplify_subdivision = 0
    eevee = scene.eevee
    if hasattr(eevee, "taa_render_samples"):
        eevee.taa_render_samples = TAA_SAMPLES
    for attr in ("use_gtao", "use_bloom", "use_ssr", "use_motion_blur"):
        if hasattr(eevee, attr):
            setattr(eevee, attr, False)


# Tracks which light rig is currently in the scene so we only rebuild on change.
_LIGHT_MODE = None


def _clear_lights() -> None:
    for obj in list(bpy.data.objects):
        if obj.type == "LIGHT":
            bpy.data.objects.remove(obj, do_unlink=True)


def _add_light(name, light_type, energy, size, location) -> None:
    light_data = bpy.data.lights.new(name=name, type=light_type)
    light_data.energy = energy
    if hasattr(light_data, "size"):
        light_data.size = size
    light_obj = bpy.data.objects.new(name, light_data)
    bpy.context.scene.collection.objects.link(light_obj)
    light_obj.location = location
    direction = Vector((0, 0, 0)) - Vector(location)
    light_obj.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()


def _add_sun(name, energy, rotation_deg) -> None:
    data = bpy.data.lights.new(name=name, type="SUN")
    data.energy = energy
    if hasattr(data, "angle"):
        data.angle = math.radians(4.0)  # softer contact shadows
    obj = bpy.data.objects.new(name, data)
    bpy.context.scene.collection.objects.link(obj)
    obj.rotation_euler = tuple(math.radians(d) for d in rotation_deg)


def setup_lighting() -> None:
    _add_light("KeyLight", "AREA", 150, 3, (2.5, -2.5, 3.5))
    _add_light("FillLight", "AREA", 60, 4, (-3, -1.5, 2))
    _add_light("RimLight", "AREA", 100, 2, (0, 3, 4))


def setup_object_lighting() -> None:
    """Sun-based three-point rig for objects.

    Suns are directional, so a prop is lit identically whether it is a coffee
    mug or a building section. Fixed-position area lights (good for clothing)
    land inside or far from off-scale props and light them unevenly.
    """
    _add_sun("KeySun", 4.0, (52, 0, 35))
    _add_sun("FillSun", 1.6, (62, 0, -120))
    _add_sun("RimSun", 2.4, (118, 0, 190))


def ensure_lighting(mode: str) -> None:
    global _LIGHT_MODE
    if _LIGHT_MODE == mode:
        return
    _clear_lights()
    if mode == "object":
        setup_object_lighting()
    else:
        setup_lighting()
    _LIGHT_MODE = mode


def setup_camera() -> "bpy.types.Object":
    cam_data = bpy.data.cameras.new("ProductCamera")
    cam_data.type = "ORTHO"
    cam_data.clip_start = 0.01
    cam_data.clip_end = 100
    cam_obj = bpy.data.objects.new("ProductCamera", cam_data)
    bpy.context.scene.collection.objects.link(cam_obj)
    bpy.context.scene.camera = cam_obj
    return cam_obj


# ---------------------------------------------------------------------------
# Import + framing helpers
# ---------------------------------------------------------------------------

def _clear_meshes() -> None:
    for obj in list(bpy.data.objects):
        if obj.type not in ("CAMERA", "LIGHT"):
            bpy.data.objects.remove(obj, do_unlink=True)
    for mesh in list(bpy.data.meshes):
        if mesh.users == 0:
            bpy.data.meshes.remove(mesh)
    for mat in list(bpy.data.materials):
        if mat.users == 0:
            bpy.data.materials.remove(mat)
    for img in list(bpy.data.images):
        if img.users == 0:
            bpy.data.images.remove(img)


def prepare_work_dir(ydd_path, dds_files, work_dir) -> str:
    ydd_name = os.path.basename(ydd_path)
    ydd_stem = os.path.splitext(ydd_name)[0]
    dest_ydd = os.path.join(work_dir, ydd_name)
    shutil.copy2(ydd_path, dest_ydd)
    tex_dir = os.path.join(work_dir, ydd_stem)
    os.makedirs(tex_dir, exist_ok=True)
    for dds_path in dds_files:
        shutil.copy2(dds_path, os.path.join(tex_dir, os.path.basename(dds_path)))
    return dest_ydd


def prepare_object_work_dir(ydr_path, work_dir) -> str:
    dest_ydr = os.path.join(work_dir, os.path.basename(ydr_path))
    shutil.copy2(ydr_path, dest_ydr)
    try:
        for entry in os.scandir(os.path.dirname(ydr_path)):
            if entry.is_file() and entry.name.lower().endswith(".ytd"):
                shutil.copy2(entry.path, os.path.join(work_dir, entry.name))
    except OSError:
        pass
    return dest_ydr


def import_drawable(path) -> bool:
    """Import a .ydd or .ydr via Sollumz (same operator for both)."""
    try:
        bpy.ops.sollumz.import_assets(
            directory=os.path.dirname(path) + os.sep,
            files=[{"name": os.path.basename(path)}],
        )
        return True
    except Exception as exc:
        print(f"Sollumz import failed: {exc}", file=sys.stderr)
        return False


def fix_missing_textures(dds_files, use_default=True) -> int:
    if not dds_files:
        return 0
    loaded = {}
    for dds_path in dds_files:
        name = os.path.splitext(os.path.basename(dds_path))[0]
        try:
            loaded[name.lower()] = bpy.data.images.load(dds_path)
        except Exception:
            pass
    if not loaded:
        return 0
    default_img = next(iter(loaded.values()))
    fixed = 0
    for mat in bpy.data.materials:
        if not mat.node_tree:
            continue
        for node in mat.node_tree.nodes:
            if node.type != "TEX_IMAGE" or node.name != "DiffuseSampler":
                continue
            img = node.image
            if img is not None and img.has_data:
                continue
            if img is not None:
                match_name = img.name.lower()
                while match_name and match_name.rsplit(".", 1)[-1].isdigit():
                    match_name = match_name.rsplit(".", 1)[0]
                if match_name.endswith(".dds"):
                    match_name = match_name[:-4]
                if match_name in loaded:
                    node.image = loaded[match_name]
                    fixed += 1
                    continue
            # Only fall back to an arbitrary texture when explicitly allowed
            # (clothing). Objects skip this so an unmatched material stays as-is
            # rather than getting the wrong texture.
            if use_default:
                node.image = default_img
                fixed += 1
    return fixed


def _is_glass_material(mat) -> bool:
    """GTA glass shaders (windows, bottles, screens) by name/shader."""
    if "glass" in (mat.name or "").lower():
        return True
    for attr in ("sollumz_shader_name", "shader_name"):
        val = getattr(mat, attr, None)
        if isinstance(val, str) and "glass" in val.lower():
            return True
    return False


def _force_material_opaque(mat) -> None:
    """Render a material fully opaque regardless of its shader graph.

    Glass uses a Sollumz node group (not a plain Principled BSDF), so we drive
    every ``Alpha`` input in the tree to 1.0 rather than only the Principled
    one, and pin the material to an opaque render method. This removes the
    dithered "missing pixel" speckle and back-face see-through under Eevee Next.
    """
    for attr, value in (
        ("surface_render_method", "DITHERED"),  # Eevee Next opaque path
        ("blend_method", "OPAQUE"),              # legacy Eevee
        ("use_raytrace_refraction", False),
        ("show_transparent_back", False),
    ):
        if hasattr(mat, attr):
            try:
                setattr(mat, attr, value)
            except Exception:
                pass
    if not mat.node_tree:
        return
    for node in mat.node_tree.nodes:
        alpha_in = getattr(node, "inputs", {}).get("Alpha") if hasattr(node, "inputs") else None
        if alpha_in is None:
            continue
        if alpha_in.is_linked:
            for link in list(mat.node_tree.links):
                if link.to_socket == alpha_in:
                    mat.node_tree.links.remove(link)
        try:
            alpha_in.default_value = 1.0
        except Exception:
            pass


def fix_alpha_modes() -> int:
    fixed = 0
    for mat in bpy.data.materials:
        if _is_glass_material(mat):
            _force_material_opaque(mat)
            fixed += 1
            continue
        # `blend_method` was removed in Blender 4.3+ (Eevee Next handles alpha
        # via the shader graph); only touch it where it still exists.
        if hasattr(mat, "blend_method") and mat.blend_method in ("BLEND", "HASHED"):
            mat.blend_method = "CLIP"
            mat.alpha_threshold = 0.01
            fixed += 1
        if mat.node_tree:
            for node in mat.node_tree.nodes:
                if node.type == "BSDF_PRINCIPLED":
                    alpha_input = node.inputs.get("Alpha")
                    if alpha_input and alpha_input.is_linked:
                        for link in list(mat.node_tree.links):
                            if link.to_socket == alpha_input:
                                mat.node_tree.links.remove(link)
                                fixed += 1
                    if alpha_input:
                        alpha_input.default_value = 1.0
    return fixed


def get_mesh_bounding_box():
    meshes = [o for o in bpy.data.objects if o.type == "MESH"]
    if not meshes:
        return None
    all_min = Vector((float("inf"),) * 3)
    all_max = Vector((float("-inf"),) * 3)
    for obj in meshes:
        for corner in obj.bound_box:
            world = obj.matrix_world @ Vector(corner)
            for i in range(3):
                all_min[i] = min(all_min[i], world[i])
                all_max[i] = max(all_max[i], world[i])
    return all_min, all_max


def is_mesh_flat(depth_ratio: float = 0.05) -> bool:
    bbox = get_mesh_bounding_box()
    if bbox is None:
        return True
    bb_min, bb_max = bbox
    size = bb_max - bb_min
    span = max(size.x, size.z)
    if span < 0.001:
        return True
    return size.y / span < depth_ratio


def frame_camera(cam_obj, azimuth_deg=0.0, elevation_deg=None) -> None:
    """Frame a clothing drawable. Azimuth 0 faces it head-on (-Y); increasing
    azimuth orbits the camera around the vertical axis for a 3/4 view."""
    bbox = get_mesh_bounding_box()
    if bbox is None:
        return
    bb_min, bb_max = bbox
    center = (bb_min + bb_max) / 2
    size = bb_max - bb_min
    elev = CAMERA_ELEVATION_DEG if elevation_deg is None else elevation_deg
    er = math.radians(elev)
    az = math.radians(azimuth_deg)
    # Projected extents at this azimuth (exactly size.x / size.y at azimuth 0).
    visible_w = abs(size.x * math.cos(az)) + abs(size.y * math.sin(az))
    depth = abs(size.x * math.sin(az)) + abs(size.y * math.cos(az))
    visible_h = size.z * math.cos(er) + depth * math.sin(er)
    cam_obj.data.ortho_scale = max(visible_w, visible_h) * PADDING_FACTOR
    distance = max(size.x, size.y, size.z, 5)
    # Horizontal view direction: azimuth 0 -> -Y (front of the piece).
    horiz = Vector((math.sin(az), -math.cos(az), 0))
    cam_obj.location = Vector((
        center.x + horiz.x * distance * math.cos(er),
        center.y + horiz.y * distance * math.cos(er),
        center.z + distance * math.sin(er),
    ))
    direction = center - cam_obj.location
    cam_obj.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()


def frame_camera_object(cam_obj, azimuth_deg=OBJECT_AZIMUTH_DEG,
                        elevation_deg=OBJECT_ELEVATION_DEG) -> None:
    bbox = get_mesh_bounding_box()
    if bbox is None:
        return
    bb_min, bb_max = bbox
    center = (bb_min + bb_max) / 2
    size = bb_max - bb_min
    az = math.radians(azimuth_deg)
    el = math.radians(elevation_deg)
    diag = math.sqrt(size.x ** 2 + size.y ** 2 + size.z ** 2)
    cam_obj.data.ortho_scale = max(diag, 0.001) * OBJECT_PADDING_FACTOR
    distance = max(size.x, size.y, size.z, 1.0) * 3.0
    offset = Vector((
        math.cos(el) * math.sin(az),
        -math.cos(el) * math.cos(az),
        math.sin(el),
    )) * distance
    cam_obj.location = center + offset
    direction = center - cam_obj.location
    cam_obj.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()


# ---------------------------------------------------------------------------
# Renderers
# ---------------------------------------------------------------------------

def render_item(item, cam_obj, work_base) -> dict:
    ydd_path = item["ydd_path"]
    dds_files = item.get("dds_files", [])
    output_path = item["output_path"]
    result = {"output_path": output_path, "success": False, "error": None}
    try:
        _clear_meshes()
        item_work = os.path.join(work_base, f"item_{id(item)}")
        os.makedirs(item_work, exist_ok=True)
        dest_ydd = prepare_work_dir(ydd_path, dds_files, item_work)
        if not import_drawable(dest_ydd):
            result["error"] = "Sollumz import failed"
            return result

        fallback_ydd = item.get("fallback_ydd")
        if fallback_ydd and is_mesh_flat():
            _clear_meshes()
            fb_work = os.path.join(work_base, f"fallback_{id(item)}")
            os.makedirs(fb_work, exist_ok=True)
            dest_fb = prepare_work_dir(fallback_ydd, dds_files, fb_work)
            if not import_drawable(dest_fb):
                result["error"] = "Fallback Sollumz import failed"
                return result

        fix_missing_textures(dds_files)
        fix_alpha_modes()
        ensure_lighting("clothing")
        # Camera angle comes from the CONFIG azimuth/elevation (the UI sliders);
        # an item-level camera_elevation still overrides if present.
        frame_camera(
            cam_obj,
            azimuth_deg=OBJECT_AZIMUTH_DEG,
            elevation_deg=item.get("camera_elevation", OBJECT_ELEVATION_DEG),
        )

        os.makedirs(os.path.dirname(output_path), exist_ok=True)
        bpy.context.scene.render.filepath = output_path
        bpy.ops.render.render(write_still=True)
        if os.path.isfile(output_path):
            result["success"] = True
        else:
            result["error"] = "Render produced no output file"
    except Exception as exc:
        result["error"] = f"{type(exc).__name__}: {exc}"
        traceback.print_exc(file=sys.stderr)
    return result


def render_object(item, cam_obj, work_base) -> dict:
    ydr_path = item["ydr_path"]
    output_path = item["output_path"]
    frames = item.get("frames")
    result = {"output_path": output_path, "success": False, "error": None, "frames": []}
    try:
        _clear_meshes()
        item_work = os.path.join(work_base, f"obj_{id(item)}")
        os.makedirs(item_work, exist_ok=True)
        dest_ydr = prepare_object_work_dir(ydr_path, item_work)
        if not import_drawable(dest_ydr):
            result["error"] = "Sollumz import failed"
            return result
        if get_mesh_bounding_box() is None:
            result["error"] = "No mesh geometry imported"
            return result
        # Force-load external .ytd textures (pre-extracted to DDS by the host)
        # that Sollumz does not auto-apply on import. Name-match only.
        dds_files = item.get("dds_files", [])
        if dds_files:
            fix_missing_textures(dds_files, use_default=False)
        fix_alpha_modes()
        ensure_lighting("object")

        if frames and int(frames) > 1:
            # Spin: render N frames over a full 360° as PNGs (for GIF assembly).
            n = int(frames)
            saved_fmt = bpy.context.scene.render.image_settings.file_format
            bpy.context.scene.render.image_settings.file_format = "PNG"
            frame_paths = []
            try:
                for i in range(n):
                    az = OBJECT_AZIMUTH_DEG + (360.0 * i / n)
                    frame_camera_object(cam_obj, az, OBJECT_ELEVATION_DEG)
                    fp = os.path.join(item_work, f"frame_{i:03d}.png")
                    bpy.context.scene.render.filepath = fp
                    bpy.ops.render.render(write_still=True)
                    if os.path.isfile(fp):
                        frame_paths.append(fp)
            finally:
                bpy.context.scene.render.image_settings.file_format = saved_fmt
            if frame_paths:
                result["frames"] = frame_paths
                result["success"] = True
            else:
                result["error"] = "Spin produced no frames"
        else:
            frame_camera_object(cam_obj, OBJECT_AZIMUTH_DEG, OBJECT_ELEVATION_DEG)
            os.makedirs(os.path.dirname(output_path), exist_ok=True)
            bpy.context.scene.render.filepath = output_path
            bpy.ops.render.render(write_still=True)
            if os.path.isfile(output_path):
                result["success"] = True
            else:
                result["error"] = "Render produced no output file"
    except Exception as exc:
        result["error"] = f"{type(exc).__name__}: {exc}"
        traceback.print_exc(file=sys.stderr)
    return result


def _wire_base_color_to_diffuse(mat) -> bool:
    """Point the Principled BSDF Base Color at the DiffuseSampler.

    GTA weapon "palette" shaders wire Base Color to a palette/tint sampler
    (e.g. W_PI_..._Dpal) whose texture ships in a shared game .ytd, not with the
    weapon. Unshipped, it stays dataless and Sollumz renders it magenta over the
    whole model. Reconnect Base Color to the real diffuse for a clean preview.
    Props whose Base Color already comes from the diffuse are left untouched.
    """
    nt = getattr(mat, "node_tree", None)
    if not nt:
        return False
    bsdf = next((n for n in nt.nodes if n.type == "BSDF_PRINCIPLED"), None)
    diffuse = next(
        (n for n in nt.nodes if n.type == "TEX_IMAGE" and n.name == "DiffuseSampler"),
        None,
    )
    if bsdf is None or diffuse is None or diffuse.image is None:
        return False
    base = bsdf.inputs.get("Base Color")
    if base is None:
        return False
    if base.is_linked and base.links[0].from_node is diffuse:
        return False
    for link in list(base.links):
        nt.links.remove(link)
    nt.links.new(diffuse.outputs["Color"], base)
    return True


def _weapon_attach_bones(armature) -> dict:
    """Map an upper-cased bone name to each of the weapon's `WAP*` attach bones
    (WAPScop, WAPClip, WAPFlshLasr, WAPSupp, ...)."""
    bones = {}
    if armature and armature.type == "ARMATURE":
        for b in armature.data.bones:
            if b.name.upper().startswith("WAP"):
                bones[b.name.upper()] = b
    return bones


def _attach_component(new_objs, weapon_arm, wap_bones) -> bool:
    """Snap a freshly imported component to its matching weapon attach bone.

    GTA weapon components carry a root bone named ``AAP<slot>`` (AAPScop,
    AAPClip, AAPFlsh, ...) authored at the origin; the weapon skeleton has the
    matching ``WAP<slot>`` bone at the real attach point. We move the component
    so its origin lands on that bone's rest transform.
    """
    comp_arm = next((o for o in new_objs if o.type == "ARMATURE"), None)
    slot = None
    if comp_arm:
        for b in comp_arm.data.bones:
            up = b.name.upper()
            if up.startswith("AAP") and len(up) > 3:
                slot = up[3:]  # e.g. "SCOP", "CLIP", "FLSH", "SUPP"
                break
    if slot is None:
        return False
    target = wap_bones.get("WAP" + slot)
    if target is None:
        target = next(
            (b for name, b in wap_bones.items() if name.startswith("WAP" + slot) or slot in name),
            None,
        )
    if target is None:
        return False
    mat = weapon_arm.matrix_world @ target.matrix_local
    for o in new_objs:
        if o.parent is None:
            o.matrix_world = mat @ o.matrix_world
    return True


def render_weapon(item, cam_obj, work_base) -> dict:
    """Import a weapon plus its attachments into one scene, snapping each
    attachment onto its matching weapon attach bone, and render a single still
    framed on the combined bounding box."""
    weapon_path = item["weapon_path"]
    attachment_paths = item.get("attachment_paths", [])
    output_path = item["output_path"]
    result = {"output_path": output_path, "success": False, "error": None}
    try:
        _clear_meshes()
        item_work = os.path.join(work_base, f"wpn_{id(item)}")
        os.makedirs(item_work, exist_ok=True)

        before = set(bpy.data.objects)
        dest_weapon = prepare_object_work_dir(weapon_path, item_work)
        if not import_drawable(dest_weapon):
            result["error"] = "Weapon Sollumz import failed"
            return result
        weapon_objs = [o for o in bpy.data.objects if o not in before]
        weapon_arm = next((o for o in weapon_objs if o.type == "ARMATURE"), None)
        wap_bones = _weapon_attach_bones(weapon_arm)

        # Attachments are optional extras: a failed one is skipped so the weapon
        # still renders. Each is snapped onto its matching weapon attach bone.
        for att in attachment_paths:
            try:
                before = set(bpy.data.objects)
                dest_att = prepare_object_work_dir(att, item_work)
                if not import_drawable(dest_att):
                    print(f"Attachment import failed: {att}", file=sys.stderr)
                    continue
                new_objs = [o for o in bpy.data.objects if o not in before]
                if weapon_arm and wap_bones and not _attach_component(new_objs, weapon_arm, wap_bones):
                    print(f"No matching attach bone for {att}; left at origin.", file=sys.stderr)
            except Exception as exc:
                print(f"Attachment error {att}: {exc}", file=sys.stderr)

        if get_mesh_bounding_box() is None:
            result["error"] = "No mesh geometry imported"
            return result

        dds_files = item.get("dds_files", [])
        if dds_files:
            fix_missing_textures(dds_files, use_default=False)
        # Weapon shaders often drive Base Color from a palette sampler whose
        # texture is not shipped with the weapon; repoint it at the diffuse.
        for mat in bpy.data.materials:
            _wire_base_color_to_diffuse(mat)
        fix_alpha_modes()
        ensure_lighting("object")

        frame_camera_object(cam_obj, OBJECT_AZIMUTH_DEG, OBJECT_ELEVATION_DEG)
        os.makedirs(os.path.dirname(output_path), exist_ok=True)
        bpy.context.scene.render.filepath = output_path
        bpy.ops.render.render(write_still=True)
        if os.path.isfile(output_path):
            result["success"] = True
        else:
            result["error"] = "Render produced no output file"
    except Exception as exc:
        result["error"] = f"{type(exc).__name__}: {exc}"
        traceback.print_exc(file=sys.stderr)
    return result


def _apply_config(config: dict) -> None:
    global RENDER_SIZE, TAA_SAMPLES, OBJECT_AZIMUTH_DEG, OBJECT_ELEVATION_DEG
    global STILL_FORMAT
    if "render_size" in config:
        RENDER_SIZE = int(config["render_size"])
    if "taa_samples" in config:
        TAA_SAMPLES = int(config["taa_samples"])
    if "azimuth" in config:
        OBJECT_AZIMUTH_DEG = float(config["azimuth"])
    if "elevation" in config:
        OBJECT_ELEVATION_DEG = float(config["elevation"])
    if "still_format" in config:
        fmt = str(config["still_format"]).upper()
        if fmt in ("WEBP", "PNG", "JPEG"):
            STILL_FORMAT = fmt


# ---------------------------------------------------------------------------
# Worker loop
# ---------------------------------------------------------------------------

def worker_main() -> None:
    clear_scene()
    setup_render_settings()
    _setup_world_ambient()
    # The light rig (area for clothing, suns for objects) is built lazily per
    # item by ensure_lighting() so each item type gets the right one.
    cam_obj = setup_camera()
    work_base = tempfile.mkdtemp(prefix="qendering_worker_")

    print("READY", flush=True)

    while True:
        try:
            line = sys.stdin.readline()
        except Exception:
            break
        if not line:
            break
        line = line.strip()
        if not line:
            continue

        if line.startswith("CONFIG:"):
            try:
                _apply_config(json.loads(line[7:]))
                setup_render_settings()
                print("CONFIG_OK", flush=True)
            except Exception as exc:
                print(f"CONFIG_ERR:{exc}", flush=True)
            continue

        try:
            item = json.loads(line)
        except json.JSONDecodeError as exc:
            print("RESULT:" + json.dumps(
                {"output_path": "", "success": False,
                 "error": f"JSON decode error: {exc}"}), flush=True)
            continue

        item_type = item.get("type")
        if item_type == "object":
            result = render_object(item, cam_obj, work_base)
        elif item_type == "weapon":
            result = render_weapon(item, cam_obj, work_base)
        else:
            result = render_item(item, cam_obj, work_base)

        print("RESULT:" + json.dumps(result), flush=True)

    try:
        shutil.rmtree(work_base, ignore_errors=True)
    except Exception:
        pass


def main() -> None:
    argv = sys.argv
    sep = argv.index("--") if "--" in argv else -1
    if sep != -1 and "--worker" in argv[sep + 1:]:
        worker_main()
        return
    print("Usage: blender -b -P blender_render.py -- --worker", file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
    main()
