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


def setup_render_settings() -> None:
    scene = bpy.context.scene
    scene.render.engine = "BLENDER_EEVEE_NEXT"
    scene.render.resolution_x = RENDER_SIZE
    scene.render.resolution_y = RENDER_SIZE
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = True
    scene.render.image_settings.file_format = "WEBP"
    scene.render.image_settings.color_mode = "RGBA"
    scene.render.image_settings.quality = 90
    scene.render.use_simplify = True
    scene.render.simplify_subdivision = 0
    eevee = scene.eevee
    if hasattr(eevee, "taa_render_samples"):
        eevee.taa_render_samples = TAA_SAMPLES
    for attr in ("use_gtao", "use_bloom", "use_ssr", "use_motion_blur"):
        if hasattr(eevee, attr):
            setattr(eevee, attr, False)


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


def setup_lighting() -> None:
    _add_light("KeyLight", "AREA", 150, 3, (2.5, -2.5, 3.5))
    _add_light("FillLight", "AREA", 60, 4, (-3, -1.5, 2))
    _add_light("RimLight", "AREA", 100, 2, (0, 3, 4))


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


def fix_missing_textures(dds_files) -> int:
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
            node.image = default_img
            fixed += 1
    return fixed


def fix_alpha_modes() -> int:
    fixed = 0
    for mat in bpy.data.materials:
        if mat.blend_method in ("BLEND", "HASHED"):
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


def frame_camera(cam_obj, elevation_deg=None) -> None:
    bbox = get_mesh_bounding_box()
    if bbox is None:
        return
    bb_min, bb_max = bbox
    center = (bb_min + bb_max) / 2
    size = bb_max - bb_min
    elev = CAMERA_ELEVATION_DEG if elevation_deg is None else elevation_deg
    er = math.radians(elev)
    visible_w = size.x
    visible_h = size.z * math.cos(er) + size.y * math.sin(er)
    cam_obj.data.ortho_scale = max(visible_w, visible_h) * PADDING_FACTOR
    distance = max(size.y, 5)
    cam_obj.location = Vector((
        center.x,
        center.y - distance * math.cos(er),
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
        frame_camera(cam_obj, elevation_deg=item.get("camera_elevation"))

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
    result = {"output_path": output_path, "success": False, "error": None}
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
        fix_alpha_modes()
        frame_camera_object(cam_obj)
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
    global RENDER_SIZE, TAA_SAMPLES
    if "render_size" in config:
        RENDER_SIZE = int(config["render_size"])
    if "taa_samples" in config:
        TAA_SAMPLES = int(config["taa_samples"])


# ---------------------------------------------------------------------------
# Worker loop
# ---------------------------------------------------------------------------

def worker_main() -> None:
    clear_scene()
    setup_render_settings()
    setup_lighting()
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

        if item.get("type") == "object":
            result = render_object(item, cam_obj, work_base)
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
