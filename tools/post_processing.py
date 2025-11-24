#!/usr/bin/env python3
import subprocess
import json
import os
import tempfile
from pathlib import Path
import math
import time
from datetime import datetime


def log(msg, level="INFO"):
    """Timestamped logging."""
    timestamp = datetime.now().strftime("%H:%M:%S")
    print(f"[{timestamp}] {level}: {msg}")


def parse_manifest(manifest_path):
    """Parse manifest and return fragment info indexed by (group_id, obj_id)."""
    log(f"Parsing manifest: {manifest_path}")
    fragments = {}
    if not os.path.exists(manifest_path):
        log(f"Manifest not found: {manifest_path}", "ERROR")
        return fragments

    with open(manifest_path, 'r') as f:
        for line in f:
            parts = line.strip().split('|')
            if len(parts) >= 5:
                group_id = parts[0]
                obj_id = parts[1]
                track_id = parts[2]
                pts = float(parts[3])
                duration = float(parts[4])

                fragments[(group_id, obj_id)] = {
                    'track_id': track_id,
                    'pts': pts,
                    'duration': duration
                }

    log(f"Parsed {len(fragments)} fragments from manifest")
    return fragments


def analyze_packet_loss(pub_fragments, sub_fragments, sync_time=None):
    """Analyze which fragments were lost during transmission (after sync time)."""
    log("Starting packet loss analysis")
    pub_keys = set(pub_fragments.keys())
    sub_keys = set(sub_fragments.keys())

    if sync_time is not None:
        pub_keys_synced = {k for k in pub_keys if pub_fragments[k]['pts'] >= sync_time}
        print(f"\nSync Point: t={sync_time:.2f}s (subscriber join time)")
        print(f"   Publisher fragments before sync: {len(pub_keys) - len(pub_keys_synced)} (excluded from analysis)")
        pub_keys = pub_keys_synced

    common = pub_keys & sub_keys
    lost = pub_keys - sub_keys
    extra = sub_keys - pub_keys

    print("\n" + "="*60)
    print("PACKET LOSS ANALYSIS")
    print("="*60)

    print(f"\nPublisher sent (after sync):      {len(pub_keys)} fragments")
    print(f"Subscriber received: {len(sub_keys)} fragments")
    print(f"Successfully received: {len(common)} fragments ({len(common)/len(pub_keys)*100:.1f}%)")
    print(f"Lost in transmission: {len(lost)} fragments ({len(lost)/len(pub_keys)*100:.1f}%)")

    if extra:
        print(f"Extra (unexpected):   {len(extra)} fragments")

    if lost:
        lost_sorted = sorted(lost, key=lambda k: pub_fragments[k]['pts'])

        print(f"\nLost Fragments (first 20):")
        for i, (g, o) in enumerate(lost_sorted[:20]):
            pts = pub_fragments[(g, o)]['pts']
            print(f"  {i+1}. g{g}/o{o} at t={pts:.2f}s")

        if len(lost) > 20:
            print(f"  ... and {len(lost) - 20} more")

        print(f"\nLoss Distribution by Time:")
        time_windows = {}
        for (g, o) in lost:
            pts = pub_fragments[(g, o)]['pts']
            window = int(pts / 1.0)
            time_windows[window] = time_windows.get(window, 0) + 1

        for window in sorted(time_windows.keys())[:10]:
            count = time_windows[window]
            print(f"  {window:3d}-{window+1:3d}s: {count:3d} losses")

    log(f"Analysis complete: {len(common)} common, {len(lost)} lost")
    return common, lost


def find_subscriber_join_time(sub_fragments):
    """Find the earliest timestamp when subscriber joined."""
    if not sub_fragments:
        log("No subscriber fragments found", "WARN")
        return 0.0

    first_frame = min(sub_fragments.values(), key=lambda f: f['pts'])
    join_time = first_frame['pts']
    log(f"Subscriber join time: {join_time:.2f}s")
    return join_time


def estimate_freeze_duration_from_manifest(pub_fragments, lost_keys, sync_time=None):
    """Estimate freeze using manifest durations (ACCURATE)."""
    log("Estimating freeze duration from manifest")

    pub_fragments_synced = pub_fragments
    if sync_time is not None:
        pub_fragments_synced = {k: v for k, v in pub_fragments.items() if v['pts'] >= sync_time}

    total_video_duration = sum(info['duration'] for info in pub_fragments_synced.values())
    total_freeze_duration = sum(pub_fragments[key]['duration'] for key in lost_keys if key in pub_fragments_synced)

    lost_sorted = sorted([k for k in lost_keys if k in pub_fragments_synced], key=lambda k: pub_fragments[k]['pts'])
    freeze_events = []

    for (group_id, obj_id) in lost_sorted:
        info = pub_fragments[(group_id, obj_id)]
        freeze_events.append({
            'group_id': group_id,
            'obj_id': obj_id,
            'pts': info['pts'],
            'duration': info['duration']
        })

    print(f"\nManifest-based freeze estimation:")
    print(f"  Total video duration: {total_video_duration:.2f}s")
    print(f"  Total freeze duration: {total_freeze_duration:.2f}s")
    print(f"  Freeze ratio: {total_freeze_duration / total_video_duration * 100:.1f}%")
    print(f"  Number of freeze events: {len(freeze_events)}")

    log(f"Freeze estimation: {total_freeze_duration:.2f}s / {total_video_duration:.2f}s ({len(freeze_events)} events)")
    return total_freeze_duration, total_video_duration, freeze_events


def calculate_lost_data_size(lost_keys, fragment_info, source_prefix):
    """Calculate total size of lost fragments in bytes."""
    log(f"Calculating size of {len(lost_keys)} lost fragments...")
    start_time = time.time()

    total_lost_bytes = 0
    fragment_sizes = {}

    for (group_id, obj_id) in lost_keys:
        info = fragment_info[(group_id, obj_id)]
        track_id = info['track_id']

        moof_path = f"{source_prefix}_moof_g{group_id}_o{obj_id}.bin"
        mdat_path = f"{source_prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

        moof_size = os.path.getsize(moof_path) if os.path.exists(moof_path) else 0
        mdat_size = os.path.getsize(mdat_path) if os.path.exists(mdat_path) else 0
        fragment_size = moof_size + mdat_size

        fragment_sizes[(group_id, obj_id)] = fragment_size
        total_lost_bytes += fragment_size

        if len(fragment_sizes) <= 10:
            print(f"  g{group_id}/o{obj_id}: {fragment_size:,} bytes (moof: {moof_size}, mdat: {mdat_size})")

    if len(lost_keys) > 10:
        print(f"  ... and {len(lost_keys) - 10} more")

    print(f"\n  Total lost data: {total_lost_bytes:,} bytes ({total_lost_bytes / 1024 / 1024:.2f} MB)")

    elapsed = time.time() - start_time
    log(f"Lost data calculation complete in {elapsed:.2f}s")

    return total_lost_bytes, fragment_sizes


def create_freeze_segment_optimized(source_video, duration, output_video, fps=24):
    """FAST freeze generation with CORRECT frame rate."""
    last_frame = "tmp/analysis/last_frame.png"

    cmd_extract = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner', '-loglevel', 'error',
        '-sseof', '-0.01', '-i', source_video,
        '-frames:v', '1', '-y', last_frame
    ]
    subprocess.run(cmd_extract, capture_output=True)

    if not os.path.exists(last_frame):
        return False

    # Force CFR + ultrafast preset for speed
    cmd_freeze = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner', '-loglevel', 'error',
        '-loop', '1',
        '-framerate', str(fps),
        '-i', last_frame,
        '-t', str(duration),
        '-c:v', 'libx264',
        '-preset', 'ultrafast',
        '-crf', '23',
        '-pix_fmt', 'yuv420p',
        '-r', str(fps),
        '-vsync', 'cfr',
        '-y', output_video
    ]
    result = subprocess.run(cmd_freeze, capture_output=True)

    if os.path.exists(last_frame):
        os.remove(last_frame)

    return result.returncode == 0


def build_video_from_fragments_optimized(fragment_keys, fragment_info, init_path,
                                         output_video, source_prefix, label, fps=24):
    """OPTIMIZED: Build video by creating ONE fMP4 stream, normalize once at end."""
    log(f"Building {label} video (optimized single-pass method)")
    start_time = time.time()

    Path("tmp/analysis").mkdir(exist_ok=True)

    # Create one big fMP4 stream (FAST binary concat)
    temp_stream = f"tmp/analysis/{label}_stream.mp4"

    total = len(fragment_keys)
    print(f"\n Building {label} video...")
    print(f"  Step 1/2: Creating fMP4 stream from {total} fragments...")
    log(f"{label}: Creating fMP4 stream from {total} fragments")

    concat_start = time.time()

    with open(temp_stream, 'wb') as stream_out:
        # Write init segment once
        with open(init_path, 'rb') as init:
            stream_out.write(init.read())

        # Append all moof+mdat fragments
        for idx, (group_id, obj_id) in enumerate(fragment_keys, 1):
            if idx % 1000 == 0 or idx == total:
                print(f"    Progress: {idx}/{total} ({idx*100//total}%)")
                log(f"{label}: Stream building progress {idx}/{total}")

            info = fragment_info[(group_id, obj_id)]
            track_id = info['track_id']

            moof_path = f"{source_prefix}_moof_g{group_id}_o{obj_id}.bin"
            mdat_path = f"{source_prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

            if os.path.exists(moof_path) and os.path.exists(mdat_path):
                with open(moof_path, 'rb') as moof:
                    stream_out.write(moof.read())
                with open(mdat_path, 'rb') as mdat:
                    stream_out.write(mdat.read())

    concat_elapsed = time.time() - concat_start
    stream_size = os.path.getsize(temp_stream)
    print(f"fMP4 stream created in {concat_elapsed:.1f}s ({stream_size:,} bytes, {stream_size/1024/1024:.2f} MB)")
    log(f"{label}: fMP4 stream created in {concat_elapsed:.1f}s")

    # Normalize ONCE at the end (veryfast preset for speed)
    print(f"  Step 2/2: Normalizing to CFR {fps}fps (veryfast preset)...")
    log(f"{label}: Starting normalization to CFR {fps}fps")
    normalize_start = time.time()

    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner', '-loglevel', 'warning', '-stats',
        '-i', temp_stream,
        '-r', str(fps),
        '-c:v', 'libx264',
        '-preset', 'veryfast',
        '-crf', '23',
        '-pix_fmt', 'yuv420p',
        '-vsync', 'cfr',
        '-movflags', '+faststart',
        '-y', output_video
    ]

    result = subprocess.run(cmd, capture_output=True, text=True)

    normalize_elapsed = time.time() - normalize_start
    print(f"Normalized in {normalize_elapsed:.1f}s ({normalize_elapsed/60:.1f} min)")
    log(f"{label}: Normalized in {normalize_elapsed:.1f}s")

    # Cleanup
    if os.path.exists(temp_stream):
        os.remove(temp_stream)

    if result.returncode != 0:
        log(f"{label}: Normalization failed: {result.stderr}", "ERROR")
        print(f"{label}: Normalization failed")
        return False

    output_size = os.path.getsize(output_video)
    print(f"{label} video complete: {output_size:,} bytes ({output_size/1024/1024:.2f} MB)")

    elapsed = time.time() - start_time
    log(f"{label} build complete in {elapsed:.2f}s ({elapsed/60:.1f} min)")

    return True


def build_video_with_manifest_based_freezing_optimized(fragment_keys, fragment_info, init_path,
                                                        output_video, source_prefix, label,
                                                        all_fragments, freeze_events,
                                                        sync_time=None, fps=24):
    """OPTIMIZED: Build video with freeze frames using single-pass method."""
    log(f"Building {label} video with freeze frames (optimized)")
    start_time = time.time()

    Path("tmp/analysis").mkdir(exist_ok=True)

    # Filter to fragments after sync time
    all_keys_sorted = sorted(all_fragments.keys(), key=lambda k: all_fragments[k]['pts'])
    if sync_time is not None:
        all_keys_sorted = [k for k in all_keys_sorted if all_fragments[k]['pts'] >= sync_time]

    received_keys = set(fragment_keys)

    print(f"\n Building {label} video with manifest-based freeze frames...")
    if sync_time is not None:
        print(f"   Starting from t={sync_time:.2f}s (subscriber join time)")

    total = len(all_keys_sorted)
    print(f"   Processing {total} fragments (received + freeze frames)...")
    log(f"{label}: Will process {total} fragments total")

    # Step 1: Create fMP4 stream for received fragments
    temp_stream = f"tmp/analysis/{label}_received_stream.mp4"

    print(f"  Step 1/3: Creating fMP4 stream for received fragments...")
    stream_start = time.time()

    with open(temp_stream, 'wb') as stream_out:
        with open(init_path, 'rb') as init:
            stream_out.write(init.read())

        received_count = 0
        for idx, (group_id, obj_id) in enumerate(all_keys_sorted, 1):
            if (group_id, obj_id) not in received_keys:
                continue

            if received_count % 500 == 0:
                print(f"    Received fragments: {received_count}")

            info = all_fragments[(group_id, obj_id)]
            track_id = info['track_id']

            moof_path = f"{source_prefix}_moof_g{group_id}_o{obj_id}.bin"
            mdat_path = f"{source_prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

            if os.path.exists(moof_path) and os.path.exists(mdat_path):
                with open(moof_path, 'rb') as moof:
                    stream_out.write(moof.read())
                with open(mdat_path, 'rb') as mdat:
                    stream_out.write(mdat.read())
                received_count += 1

    stream_elapsed = time.time() - stream_start
    print(f"Received stream created in {stream_elapsed:.1f}s ({received_count} fragments)")

    # Step 2: Normalize received stream
    print(f"  Step 2/3: Normalizing received stream to CFR {fps}fps...")
    temp_normalized = f"tmp/analysis/{label}_normalized.mp4"
    normalize_start = time.time()

    cmd_normalize = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner', '-loglevel', 'warning',
        '-i', temp_stream,
        '-r', str(fps),
        '-c:v', 'libx264',
        '-preset', 'veryfast',
        '-crf', '23',
        '-pix_fmt', 'yuv420p',
        '-vsync', 'cfr',
        '-y', temp_normalized
    ]

    result = subprocess.run(cmd_normalize, capture_output=True)
    os.remove(temp_stream)

    if result.returncode != 0:
        log(f"{label}: Normalization failed", "ERROR")
        return False

    normalize_elapsed = time.time() - normalize_start
    print(f"Normalized in {normalize_elapsed:.1f}s")

    # Step 3: Insert freeze frames
    print(f"  Step 3/3: Inserting {len(freeze_events)} freeze frames...")
    freeze_start = time.time()

    # Create freeze segments
    freeze_files = []
    last_frame_source = temp_normalized

    for idx, event in enumerate(freeze_events):
        if idx % 50 == 0:
            print(f"    Creating freeze frames: {idx}/{len(freeze_events)}")

        freeze_file = f"tmp/analysis/{label}_freeze_{idx}.mp4"
        if create_freeze_segment_optimized(last_frame_source, event['duration'], freeze_file, fps):
            freeze_files.append(freeze_file)

    freeze_elapsed = time.time() - freeze_start
    print(f"Created {len(freeze_files)} freeze segments in {freeze_elapsed:.1f}s")

    # Interleave received and freeze (simplified: just append all freeze at end for now)
    # Full interleaving would require complex timestamp tracking
    print(f"Concatenating received video + freeze frames...")

    all_segments = [temp_normalized] + freeze_files

    # Simple concat
    with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
        concat_list = f.name
        for seg in all_segments:
            f.write(f"file '{os.path.abspath(seg)}'\n")

    cmd_concat = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner', '-loglevel', 'error',
        '-f', 'concat', '-safe', '0',
        '-i', concat_list,
        '-c', 'copy',
        '-y', output_video
    ]

    result = subprocess.run(cmd_concat, capture_output=True)
    os.remove(concat_list)

    # Cleanup
    if os.path.exists(temp_normalized):
        os.remove(temp_normalized)
    for freeze_file in freeze_files:
        if os.path.exists(freeze_file):
            os.remove(freeze_file)

    if result.returncode != 0:
        log(f"{label}: Final concat failed", "ERROR")
        return False

    output_size = os.path.getsize(output_video)
    print(f"{label} video complete: {output_size:,} bytes ({output_size/1024/1024:.2f} MB)")

    elapsed = time.time() - start_time
    log(f"{label} build complete in {elapsed:.2f}s ({elapsed/60:.1f} min)")

    return True


def build_full_video_from_common_fragments(pub_manifest, sub_manifest, init_path,
                                           pub_output, sub_output, pub_prefix, sub_prefix,
                                           sync_time=None, fps=24):
    """Build QoS videos from common fragments (optimized)."""
    log("Building QoS videos from common fragments")

    pub_fragments = parse_manifest(pub_manifest)
    sub_fragments = parse_manifest(sub_manifest)

    common, lost = analyze_packet_loss(pub_fragments, sub_fragments, sync_time)

    if not common:
        log("No common fragments found", "ERROR")
        print("No common fragments found!")
        return False, False, {}

    common_sorted = sorted(common, key=lambda k: pub_fragments[k]['pts'])

    print(f"\n" + "="*60)
    print(f"BUILDING QoS VIDEOS ({len(common_sorted)} common fragments)")
    print("="*60)

    success_pub = build_video_from_fragments_optimized(
        common_sorted, pub_fragments, init_path, pub_output, pub_prefix, "Publisher_QoS", fps
    )

    success_sub = build_video_from_fragments_optimized(
        common_sorted, sub_fragments, init_path, sub_output, sub_prefix, "Subscriber_QoS", fps
    )

    # Calculate stats
    pub_keys_synced = pub_fragments.keys()
    if sync_time is not None:
        pub_keys_synced = [k for k in pub_keys_synced if pub_fragments[k]['pts'] >= sync_time]

    stats = {
        'total_sent': len(pub_keys_synced),
        'total_received': len(sub_fragments),
        'common': len(common),
        'lost': len(lost),
        'loss_rate': len(lost) / len(pub_keys_synced) * 100 if pub_keys_synced else 0
    }

    log(f"QoS video build complete (Pub: {success_pub}, Sub: {success_sub})")

    return success_pub, success_sub, stats


def run_quality_metrics(distorted_path, reference_path, vmaf_json, psnr_log, ssim_log):
    """Run FFmpeg with VMAF, PSNR, and SSIM analysis."""
    log("Starting quality metrics analysis (VMAF, PSNR, SSIM)")
    start_time = time.time()

    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner',
        '-i', distorted_path,
        '-i', reference_path,
        '-lavfi', f'[0:v][1:v]psnr=stats_file={psnr_log}[psnr];[psnr][1:v]ssim=stats_file={ssim_log}[ssim];[ssim][1:v]libvmaf=log_fmt=json:log_path={vmaf_json}:n_threads=4',
        '-f', 'null', '-'
    ]

    print(f"\nRunning quality metrics (VMAF, PSNR, SSIM)...")
    print(f"   This may take several minutes...")

    result = subprocess.run(cmd, capture_output=True, text=True)

    elapsed = time.time() - start_time
    log(f"Quality metrics analysis complete in {elapsed:.2f}s ({elapsed/60:.1f} min)")

    if result.returncode != 0:
        log(f"FFmpeg quality analysis failed: {result.stderr}", "ERROR")
        print(f"FFmpeg quality analysis failed")
        return None, None, None

    vmaf_score = None
    if os.path.exists(vmaf_json):
        try:
            with open(vmaf_json, 'r') as f:
                data = json.load(f)
                vmaf_score = data['pooled_metrics']['vmaf']['mean']
                log(f"VMAF score: {vmaf_score:.2f}")
        except (json.JSONDecodeError, KeyError) as e:
            log(f"Failed to parse VMAF: {e}", "ERROR")

    psnr_score = None
    if os.path.exists(psnr_log):
        try:
            with open(psnr_log, 'r') as f:
                last_line = f.readlines()[-1]
                if 'psnr_avg:' in last_line:
                    psnr_score = float(last_line.split('psnr_avg:')[1].split()[0])
                    log(f"PSNR score: {psnr_score:.2f} dB")
        except (IndexError, ValueError) as e:
            log(f"Failed to parse PSNR: {e}", "ERROR")

    ssim_score = None
    if os.path.exists(ssim_log):
        try:
            with open(ssim_log, 'r') as f:
                last_line = f.readlines()[-1]
                if 'All:' in last_line:
                    ssim_score = float(last_line.split('All:')[1].split()[0])
                    log(f"SSIM score: {ssim_score:.4f}")
        except (IndexError, ValueError) as e:
            log(f"Failed to parse SSIM: {e}", "ERROR")

    return vmaf_score, psnr_score, ssim_score


def calculate_qoe_score(vmaf, loss_rate, freeze_duration_sec, video_duration_sec):
    """Calculate perceptual QoE score."""
    qoe = vmaf
    qoe *= (1.0 - loss_rate * 0.3)
    freeze_ratio = freeze_duration_sec / video_duration_sec if video_duration_sec > 0 else 0
    qoe -= freeze_ratio * 50.0
    final_qoe = max(0, min(100, qoe))

    log(f"QoE calculation: VMAF={vmaf:.2f}, loss_penalty={loss_rate*0.3:.3f}, freeze_penalty={freeze_ratio*50:.2f} -> QoE={final_qoe:.2f}")

    return final_qoe


def main():
    log("="*60)
    log("STARTING OPTIMIZED VIDEO QUALITY ANALYSIS")
    log("="*60)

    overall_start = time.time()

    track_id = 1
    fps = 24

    pub_manifest = f"tmp/pub_manifest_track{track_id}.txt"
    sub_manifest = f"tmp/sub/sub_manifest_track{track_id}.txt"
    init_path = f"tmp/init_track{track_id}.mp4"

    if not os.path.exists(pub_manifest) or not os.path.exists(sub_manifest):
        log("Manifest files not found", "ERROR")
        print("Manifest files not found")
        return

    Path("tmp/analysis").mkdir(exist_ok=True)

    pub_fragments = parse_manifest(pub_manifest)
    sub_fragments = parse_manifest(sub_manifest)

    sync_time = find_subscriber_join_time(sub_fragments)
    print(f"\nSubscriber joined at t={sync_time:.2f}s")

    common, lost = analyze_packet_loss(pub_fragments, sub_fragments, sync_time)

    lost_sorted = sorted(lost, key=lambda k: pub_fragments[k]['pts'])
    total_lost_bytes, lost_sizes = calculate_lost_data_size(
        lost_sorted, pub_fragments, "tmp/pub"
    )

    manifest_freeze_duration, manifest_video_duration, freeze_events = \
        estimate_freeze_duration_from_manifest(pub_fragments, lost_sorted, sync_time)

    # PART 1: QoS
    print("\n" + "="*60)
    print("PART 1: QoS MEASUREMENT")
    print("="*60)
    log("Starting QoS measurement phase")

    pub_qos_video = "tmp/analysis/pub_qos.mp4"
    sub_qos_video = "tmp/analysis/sub_qos.mp4"

    success_pub_qos, success_sub_qos, stats = build_full_video_from_common_fragments(
        pub_manifest, sub_manifest, init_path,
        pub_qos_video, sub_qos_video,
        "tmp/pub", "tmp/sub/sub",
        sync_time, fps
    )

    qos_vmaf = qos_psnr = qos_ssim = None
    if success_pub_qos and success_sub_qos:
        qos_vmaf, qos_psnr, qos_ssim = run_quality_metrics(
            sub_qos_video, pub_qos_video,
            "tmp/analysis/vmaf_qos.json",
            "tmp/analysis/psnr_qos.log",
            "tmp/analysis/ssim_qos.log"
        )

    # PART 2: QoE
    print("\n" + "="*60)
    print("PART 2: QoE MEASUREMENT")
    print("="*60)
    log("Starting QoE measurement phase")

    pub_qoe_video = "tmp/analysis/pub_qoe_reference.mp4"
    sub_qoe_video = "tmp/analysis/sub_qoe_with_freezing.mp4"

    pub_keys_synced = sorted([k for k in pub_fragments.keys() if pub_fragments[k]['pts'] >= sync_time],
                             key=lambda k: pub_fragments[k]['pts'])

    success_pub_ref = build_video_from_fragments_optimized(
        pub_keys_synced, pub_fragments, init_path,
        pub_qoe_video, "tmp/pub", "Publisher_Reference", fps
    )

    success_sub_qoe = build_video_with_manifest_based_freezing_optimized(
        list(common), sub_fragments, init_path,
        sub_qoe_video, "tmp/sub/sub", "Subscriber_QoE",
        pub_fragments, freeze_events, sync_time, fps
    )

    qoe_vmaf = qoe_psnr = qoe_ssim = perceptual_qoe = None
    if success_pub_ref and success_sub_qoe:
        qoe_vmaf, qoe_psnr, qoe_ssim = run_quality_metrics(
            sub_qoe_video, pub_qoe_video,
            "tmp/analysis/vmaf_qoe.json",
            "tmp/analysis/psnr_qoe.log",
            "tmp/analysis/ssim_qoe.log"
        )

        if qoe_vmaf is not None:
            perceptual_qoe = calculate_qoe_score(
                qoe_vmaf, stats['loss_rate'] / 100,
                manifest_freeze_duration, manifest_video_duration
            )

    # RESULTS
    print("\n" + "="*60)
    print("FINAL RESULTS")
    print("="*60)

    print(f"\nQoS Metrics:")
    if qos_vmaf is not None:
        print(f"  VMAF:  {qos_vmaf:.2f}")
        if qos_psnr:
            print(f"  PSNR:  {qos_psnr:.2f} dB")
        if qos_ssim:
            print(f"  SSIM:  {qos_ssim:.4f}")

    print(f"\nQoE Metrics:")
    if qoe_vmaf is not None:
        print(f"  VMAF:  {qoe_vmaf:.2f}")
        if qoe_psnr:
            print(f"  PSNR:  {qoe_psnr:.2f} dB")
        if qoe_ssim:
            print(f"  SSIM:  {qoe_ssim:.4f}")
        if perceptual_qoe is not None:
            print(f"  QoE Score: {perceptual_qoe:.2f}")

    print(f"\nDelivery Metrics:")
    print(f"  Delivery Rate:    {100 - stats['loss_rate']:.1f}%")
    print(f"  Packet Loss:      {stats['loss_rate']:.1f}%")
    print(f"  Freeze Events:    {len(freeze_events)}")
    print(f"  Freeze Duration:  {manifest_freeze_duration:.2f}s / {manifest_video_duration:.2f}s ({manifest_freeze_duration / manifest_video_duration * 100:.1f}%)")
    print(f"  Lost Data Size:   {total_lost_bytes / 1024 / 1024:.2f} MB")
    print(f"  Sync Time:        {sync_time:.2f}s")

    if freeze_events:
        print(f"\nFreeze Events (first 10):")
        for i, event in enumerate(freeze_events[:10]):
            print(f"  {i+1}. g{event['group_id']}/o{event['obj_id']}: "
                  f"{event['duration']:.3f}s at t={event['pts']:.2f}s")
        if len(freeze_events) > 10:
            print(f"  ... and {len(freeze_events) - 10} more")

    results = {
        'qos': {'vmaf': qos_vmaf, 'psnr': qos_psnr, 'ssim': qos_ssim},
        'qoe': {
            'vmaf': qoe_vmaf,
            'psnr': qoe_psnr,
            'ssim': qoe_ssim,
            'perceptual_score': perceptual_qoe,
            'freeze_events': len(freeze_events),
            'freeze_duration_sec': manifest_freeze_duration,
            'video_duration_sec': manifest_video_duration,
            'freeze_ratio': manifest_freeze_duration / manifest_video_duration if manifest_video_duration > 0 else 0,
        },
        'delivery': {
            'total_sent': stats['total_sent'],
            'total_received': stats['total_received'],
            'loss_rate_percent': stats['loss_rate'],
            'lost_bytes': total_lost_bytes,
            'sync_time_sec': sync_time,
        },
    }

    with open('tmp/comprehensive_results.json', 'w') as f:
        json.dump(results, f, indent=2)

    print(f"\nResults saved to tmp/comprehensive_results.json")
    log("Results saved to tmp/comprehensive_results.json")

    overall_elapsed = time.time() - overall_start
    log("="*60)
    log(f"ANALYSIS COMPLETE - Total time: {overall_elapsed:.2f}s ({overall_elapsed/60:.1f} minutes)")
    log("="*60)


if __name__ == '__main__':
    main()
