#!/usr/bin/env python3
import subprocess
import json
import os
import tempfile
from pathlib import Path
import math


def parse_manifest(manifest_path):
    """Parse manifest and return fragment info indexed by (group_id, obj_id)."""
    fragments = {}
    if not os.path.exists(manifest_path):
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
    return fragments


def analyze_packet_loss(pub_fragments, sub_fragments):
    """Analyze which fragments were lost during transmission."""
    pub_keys = set(pub_fragments.keys())
    sub_keys = set(sub_fragments.keys())

    common = pub_keys & sub_keys
    lost = pub_keys - sub_keys
    extra = sub_keys - pub_keys

    print("\n" + "="*60)
    print("📊 PACKET LOSS ANALYSIS")
    print("="*60)

    print(f"\n📤 Publisher sent:      {len(pub_keys)} fragments")
    print(f"📥 Subscriber received: {len(sub_keys)} fragments")
    print(f"✅ Successfully received: {len(common)} fragments ({len(common)/len(pub_keys)*100:.1f}%)")
    print(f"❌ Lost in transmission: {len(lost)} fragments ({len(lost)/len(pub_keys)*100:.1f}%)")

    if extra:
        print(f"⚠️ Extra (unexpected):   {len(extra)} fragments")

    if lost:
        lost_sorted = sorted(lost, key=lambda k: pub_fragments[k]['pts'])

        print(f"\n❌ Lost Fragments (first 20):")
        for i, (g, o) in enumerate(lost_sorted[:20]):
            pts = pub_fragments[(g, o)]['pts']
            print(f"  {i+1}. g{g}/o{o} at t={pts:.2f}s")

        if len(lost) > 20:
            print(f"  ... and {len(lost) - 20} more")

        print(f"\n📉 Loss Distribution by Time:")
        time_windows = {}
        for (g, o) in lost:
            pts = pub_fragments[(g, o)]['pts']
            window = int(pts / 1.0)
            time_windows[window] = time_windows.get(window, 0) + 1

        for window in sorted(time_windows.keys())[:10]:
            count = time_windows[window]
            print(f"  {window:3d}-{window+1:3d}s: {count:3d} losses")

    return common, lost


def create_fragment_segment(group_id, obj_id, info, init_path, prefix, output):
    """Create fragment segment - FAST binary concat."""
    track_id = info['track_id']
    moof_path = f"{prefix}_moof_g{group_id}_o{obj_id}.bin"
    mdat_path = f"{prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

    if not os.path.exists(moof_path) or not os.path.exists(mdat_path):
        return False

    with open(output, 'wb') as out:
        with open(init_path, 'rb') as init:
            out.write(init.read())
        with open(moof_path, 'rb') as moof:
            out.write(moof.read())
        with open(mdat_path, 'rb') as mdat:
            out.write(mdat.read())

    return True


def batch_concat_videos(video_files, output_path, batch_size=100):
    """Hierarchical concat to avoid 'Too many open files' error."""
    if len(video_files) <= batch_size:
        # Direct concat
        return simple_concat_videos(video_files, output_path)

    print(f"  📦 Batching {len(video_files)} files into groups of {batch_size}...")

    # Step 1: Concat in batches
    batch_outputs = []
    num_batches = math.ceil(len(video_files) / batch_size)

    for i in range(num_batches):
        start_idx = i * batch_size
        end_idx = min((i + 1) * batch_size, len(video_files))
        batch = video_files[start_idx:end_idx]

        batch_output = f"tmp/analysis/batch_{i}.mp4"
        if simple_concat_videos(batch, batch_output):
            batch_outputs.append(batch_output)
            print(f"  ✓ Batch {i+1}/{num_batches} done ({len(batch)} files)")

    # Step 2: Concat all batches
    print(f"  🔗 Merging {len(batch_outputs)} batches...")
    success = simple_concat_videos(batch_outputs, output_path)

    # Cleanup batch files
    for batch_file in batch_outputs:
        if os.path.exists(batch_file):
            os.remove(batch_file)

    return success


def simple_concat_videos(video_files, output_path):
    """Simple concat using concat demuxer."""
    with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as f:
        concat_list = f.name
        for video_file in video_files:
            f.write(f"file '{os.path.abspath(video_file)}'\n")

    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner',
        '-f', 'concat',
        '-safe', '0',
        '-i', concat_list,
        '-c', 'copy',
        '-y', output_path
    ]

    result = subprocess.run(cmd, capture_output=True, text=True)
    os.remove(concat_list)

    return result.returncode == 0


def build_video_with_freezing_batched(fragment_keys, fragment_info, init_path,
                                      output_video, source_prefix, label, all_fragments):
    """OPTIMIZED: Build video with freezing using batched approach."""
    Path("tmp/analysis").mkdir(exist_ok=True)

    all_keys_sorted = sorted(all_fragments.keys(), key=lambda k: all_fragments[k]['pts'])
    received_keys = set(fragment_keys)

    video_segments = []
    freeze_events = []
    temp_files = []

    print(f"\n🔧 Building {label} video (batched approach for {len(all_keys_sorted)} fragments)...")

    last_valid_segment = None

    for (group_id, obj_id) in all_keys_sorted:
        info = all_fragments[(group_id, obj_id)]

        if (group_id, obj_id) in received_keys:
            # Create normal segment
            seg_file = f"tmp/analysis/{label}_g{group_id}_o{obj_id}.mp4"
            if create_fragment_segment(group_id, obj_id, info, init_path,
                                      source_prefix, seg_file):
                video_segments.append(seg_file)
                temp_files.append(seg_file)
                last_valid_segment = seg_file
        else:
            # Create freeze frame segment
            if last_valid_segment:
                freeze_file = f"tmp/analysis/{label}_freeze_g{group_id}_o{obj_id}.mp4"
                if create_freeze_segment_fast(last_valid_segment, info['duration'], freeze_file):
                    video_segments.append(freeze_file)
                    temp_files.append(freeze_file)
                    freeze_events.append({
                        'pts': info['pts'],
                        'duration': info['duration'],
                        'group_id': group_id,
                        'obj_id': obj_id
                    })

    if not video_segments:
        print(f"  ❌ {label}: No segments")
        return False, []

    print(f"  🔗 Concatenating {len(video_segments)} segments (batched)...")

    # Use batched concat to avoid "Too many open files"
    success = batch_concat_videos(video_segments, output_video, batch_size=100)

    # Cleanup
    for temp_file in temp_files:
        if os.path.exists(temp_file):
            os.remove(temp_file)

    if not success:
        print(f"  ❌ {label}: Concat failed")
        return False, []

    total_freeze_duration = sum(e['duration'] for e in freeze_events)
    print(f"  ✅ {label} video: {os.path.getsize(output_video):,} bytes")
    print(f"  ⏸️ Freeze events: {len(freeze_events)} ({total_freeze_duration:.2f}s total)")

    return True, freeze_events


def create_freeze_segment_fast(source_video, duration, output_video):
    """Create a freeze segment by looping last frame."""
    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner',
        '-sseof', '-0.01',
        '-i', source_video,
        '-vf', f'loop=loop=-1:size=1,setpts=N/FRAME_RATE/TB',
        '-t', str(duration),
        '-c:v', 'libx264',
        '-preset', 'ultrafast',
        '-crf', '23',
        '-y', output_video
    ]

    result = subprocess.run(cmd, capture_output=True, text=True)
    return result.returncode == 0


def build_full_video_from_common_fragments(pub_manifest, sub_manifest, init_path,
                                           pub_output, sub_output, pub_prefix, sub_prefix):
    """Build videos ONLY from common fragments."""
    pub_fragments = parse_manifest(pub_manifest)
    sub_fragments = parse_manifest(sub_manifest)

    common, lost = analyze_packet_loss(pub_fragments, sub_fragments)

    if not common:
        print("❌ No common fragments found!")
        return False, False, {}

    common_sorted = sorted(common, key=lambda k: pub_fragments[k]['pts'])

    print(f"\n🔧 Building QoS videos from {len(common_sorted)} common fragments...")

    success_pub = build_video_from_fragments(
        common_sorted, pub_fragments, init_path, pub_output, pub_prefix, "Publisher_QoS"
    )

    success_sub = build_video_from_fragments(
        common_sorted, sub_fragments, init_path, sub_output, sub_prefix, "Subscriber_QoS"
    )

    stats = {
        'total_sent': len(pub_fragments),
        'total_received': len(sub_fragments),
        'common': len(common),
        'lost': len(lost),
        'loss_rate': len(lost) / len(pub_fragments) * 100 if pub_fragments else 0
    }

    return success_pub, success_sub, stats


def build_video_from_fragments(fragment_keys, fragment_info, init_path,
                               output_video, source_prefix, label):
    """Build video from fragments using batched concat."""
    Path("tmp/analysis").mkdir(exist_ok=True)

    temp_files = []
    for (group_id, obj_id) in fragment_keys:
        info = fragment_info[(group_id, obj_id)]
        track_id = info['track_id']

        moof_path = f"{source_prefix}_moof_g{group_id}_o{obj_id}.bin"
        mdat_path = f"{source_prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

        if not os.path.exists(moof_path) or not os.path.exists(mdat_path):
            continue

        temp_frag = f"tmp/analysis/temp_{label}_g{group_id}_o{obj_id}.mp4"
        with open(temp_frag, 'wb') as out:
            with open(init_path, 'rb') as init:
                out.write(init.read())
            with open(moof_path, 'rb') as moof:
                out.write(moof.read())
            with open(mdat_path, 'rb') as mdat:
                out.write(mdat.read())

        temp_files.append(temp_frag)

    if not temp_files:
        print(f"  ❌ {label}: No valid fragments")
        return False

    # Use batched concat
    success = batch_concat_videos(temp_files, output_video, batch_size=100)

    for temp_file in temp_files:
        if os.path.exists(temp_file):
            os.remove(temp_file)

    if not success:
        print(f"  ❌ {label}: Concat failed")
        return False

    print(f"  ✅ {label} video: {os.path.getsize(output_video):,} bytes ({len(temp_files)} fragments)")
    return True


def run_quality_metrics(distorted_path, reference_path, vmaf_json, psnr_log, ssim_log):
    """Run FFmpeg with VMAF, PSNR, and SSIM analysis."""
    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner',
        '-i', distorted_path,
        '-i', reference_path,
        '-lavfi', f'[0:v][1:v]psnr=stats_file={psnr_log}[psnr];[psnr][1:v]ssim=stats_file={ssim_log}[ssim];[ssim][1:v]libvmaf=log_fmt=json:log_path={vmaf_json}:n_threads=4',
        '-f', 'null', '-'
    ]

    print(f"\n🔧 Running quality metrics (VMAF, PSNR, SSIM)...")
    result = subprocess.run(cmd, capture_output=True, text=True)

    if result.returncode != 0:
        print(f"❌ FFmpeg quality analysis failed")
        print(f"STDERR: {result.stderr[-500:]}")  # Last 500 chars
        return None, None, None

    vmaf_score = None
    if os.path.exists(vmaf_json):
        try:
            with open(vmaf_json, 'r') as f:
                data = json.load(f)
                vmaf_score = data['pooled_metrics']['vmaf']['mean']
        except (json.JSONDecodeError, KeyError) as e:
            print(f"❌ Failed to parse VMAF: {e}")

    psnr_score = None
    if os.path.exists(psnr_log):
        try:
            with open(psnr_log, 'r') as f:
                last_line = f.readlines()[-1]
                if 'psnr_avg:' in last_line:
                    psnr_score = float(last_line.split('psnr_avg:')[1].split()[0])
        except (IndexError, ValueError) as e:
            print(f"❌ Failed to parse PSNR: {e}")

    ssim_score = None
    if os.path.exists(ssim_log):
        try:
            with open(ssim_log, 'r') as f:
                last_line = f.readlines()[-1]
                if 'All:' in last_line:
                    ssim_score = float(last_line.split('All:')[1].split()[0])
        except (IndexError, ValueError) as e:
            print(f"❌ Failed to parse SSIM: {e}")

    return vmaf_score, psnr_score, ssim_score


def calculate_qoe_score(vmaf, loss_rate, freeze_duration_sec, video_duration_sec):
    """Calculate perceptual QoE score."""
    qoe = vmaf
    qoe *= (1.0 - loss_rate * 0.3)
    freeze_ratio = freeze_duration_sec / video_duration_sec if video_duration_sec > 0 else 0
    qoe -= freeze_ratio * 50.0
    return max(0, min(100, qoe))


def main():
    track_id = 1
    pub_manifest = f"tmp/pub_manifest_track{track_id}.txt"
    sub_manifest = f"tmp/sub/sub_manifest_track{track_id}.txt"
    init_path = f"tmp/init_track{track_id}.mp4"

    if not os.path.exists(pub_manifest) or not os.path.exists(sub_manifest):
        print("❌ Manifest files not found")
        return

    Path("tmp/analysis").mkdir(exist_ok=True)

    pub_fragments = parse_manifest(pub_manifest)
    sub_fragments = parse_manifest(sub_manifest)
    common, lost = analyze_packet_loss(pub_fragments, sub_fragments)

    # PART 1: QoS
    print("\n" + "="*60)
    print("📊 PART 1: QoS MEASUREMENT")
    print("="*60)

    pub_qos_video = "tmp/analysis/pub_qos.mp4"
    sub_qos_video = "tmp/analysis/sub_qos.mp4"

    success_pub_qos, success_sub_qos, stats = build_full_video_from_common_fragments(
        pub_manifest, sub_manifest, init_path,
        pub_qos_video, sub_qos_video,
        "tmp/pub", "tmp/sub/sub"
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
    print("📊 PART 2: QoE MEASUREMENT")
    print("="*60)

    pub_qoe_video = "tmp/analysis/pub_qoe_reference.mp4"
    sub_qoe_video = "tmp/analysis/sub_qoe_with_freezing.mp4"

    all_pub_keys = sorted(pub_fragments.keys(), key=lambda k: pub_fragments[k]['pts'])
    success_pub_ref = build_video_from_fragments(
        all_pub_keys, pub_fragments, init_path,
        pub_qoe_video, "tmp/pub", "Publisher_Reference"
    )

    success_sub_qoe, freeze_events = build_video_with_freezing_batched(
        list(common), sub_fragments, init_path,
        sub_qoe_video, "tmp/sub/sub", "Subscriber_QoE",
        pub_fragments
    )

    qoe_vmaf = qoe_psnr = qoe_ssim = perceptual_qoe = None
    if success_pub_ref and success_sub_qoe:
        qoe_vmaf, qoe_psnr, qoe_ssim = run_quality_metrics(
            sub_qoe_video, pub_qoe_video,
            "tmp/analysis/vmaf_qoe.json",
            "tmp/analysis/psnr_qoe.log",
            "tmp/analysis/ssim_qoe.log"
        )

    # RESULTS
    print("\n" + "="*60)
    print("📈 FINAL RESULTS")
    print("="*60)

    total_freeze_duration = sum(e['duration'] for e in freeze_events)
    video_duration = max(pub_fragments[k]['pts'] + pub_fragments[k]['duration']
                        for k in pub_fragments.keys())

    print(f"\n🎯 QoS Metrics:")
    if qos_vmaf is not None:
        print(f"  VMAF:  {qos_vmaf:.2f}")
        if qos_psnr:
            print(f"  PSNR:  {qos_psnr:.2f} dB")
        if qos_ssim:
            print(f"  SSIM:  {qos_ssim:.4f}")

    print(f"\n🎯 QoE Metrics:")
    if qoe_vmaf is not None:
        perceptual_qoe = calculate_qoe_score(
            qoe_vmaf, stats['loss_rate'] / 100,
            total_freeze_duration, video_duration
        )
        print(f"  VMAF:  {qoe_vmaf:.2f}")
        if qoe_psnr:
            print(f"  PSNR:  {qoe_psnr:.2f} dB")
        if qoe_ssim:
            print(f"  SSIM:  {qoe_ssim:.4f}")
        print(f"  QoE Score: {perceptual_qoe:.2f}")

    print(f"\n📊 Delivery Metrics:")
    print(f"  Delivery Rate:    {100 - stats['loss_rate']:.1f}%")
    print(f"  Packet Loss:      {stats['loss_rate']:.1f}%")
    print(f"  Freeze Events:    {len(freeze_events)}")
    print(f"  Freeze Duration:  {total_freeze_duration:.2f}s / {video_duration:.2f}s")

    results = {
        'qos': {'vmaf': qos_vmaf, 'psnr': qos_psnr, 'ssim': qos_ssim},
        'qoe': {
            'vmaf': qoe_vmaf,
            'psnr': qoe_psnr,
            'ssim': qoe_ssim,
            'perceptual_score': perceptual_qoe,
            'freeze_events': len(freeze_events),
            'freeze_duration_sec': total_freeze_duration,
        },
        'delivery': {
            'total_sent': stats['total_sent'],
            'total_received': stats['total_received'],
            'loss_rate_percent': stats['loss_rate'],
        },
    }

    with open('tmp/comprehensive_results.json', 'w') as f:
        json.dump(results, f, indent=2)

    print(f"\n📁 Results saved")


if __name__ == '__main__':
    main()
