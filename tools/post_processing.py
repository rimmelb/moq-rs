#!/usr/bin/env python3
import subprocess
import json
import os
import tempfile
from pathlib import Path


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

    # Common fragments (received)
    common = pub_keys & sub_keys

    # Lost fragments (sent but not received)
    lost = pub_keys - sub_keys

    # Extra fragments (received but not sent - should be empty)
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

    # Show lost fragments sorted by time
    if lost:
        lost_sorted = sorted(lost, key=lambda k: pub_fragments[k]['pts'])

        print(f"\n❌ Lost Fragments (first 20):")
        for i, (g, o) in enumerate(lost_sorted[:20]):
            pts = pub_fragments[(g, o)]['pts']
            print(f"  {i+1}. g{g}/o{o} at t={pts:.2f}s")

        if len(lost) > 20:
            print(f"  ... and {len(lost) - 20} more")

        # Group losses by time windows
        print(f"\n📉 Loss Distribution by Time:")
        time_windows = {}
        for (g, o) in lost:
            pts = pub_fragments[(g, o)]['pts']
            window = int(pts / 1.0)  # 1-second windows
            time_windows[window] = time_windows.get(window, 0) + 1

        for window in sorted(time_windows.keys())[:10]:
            count = time_windows[window]
            print(f"  {window:3d}-{window+1:3d}s: {count:3d} losses")

    return common, lost


def build_full_video_from_common_fragments(pub_manifest, sub_manifest, init_path,
                                           pub_output, sub_output, pub_prefix, sub_prefix):
    """Build videos ONLY from common fragments, sorted by PTS."""

    # Parse both manifests
    pub_fragments = parse_manifest(pub_manifest)
    sub_fragments = parse_manifest(sub_manifest)

    # Analyze packet loss
    common, lost = analyze_packet_loss(pub_fragments, sub_fragments)

    if not common:
        print("❌ No common fragments found!")
        return False, False, {}

    # Sort by PTS to maintain temporal order
    common_sorted = sorted(common, key=lambda k: pub_fragments[k]['pts'])

    print(f"\n🔧 Building time-aligned videos from {len(common_sorted)} common fragments...")

    # Build publisher video from common fragments
    success_pub = build_video_from_fragments(
        common_sorted, pub_fragments, init_path, pub_output, pub_prefix, "Publisher"
    )

    # Build subscriber video from common fragments
    success_sub = build_video_from_fragments(
        common_sorted, sub_fragments, init_path, sub_output, sub_prefix, "Subscriber"
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
    """Build a video from specified fragments in PTS order."""

    Path("tmp/analysis").mkdir(exist_ok=True)

    # Create temporary concat list
    with tempfile.NamedTemporaryFile(mode='w', suffix='.txt', delete=False) as concat_file:
        concat_path = concat_file.name
        temp_files = []
        missing_count = 0

        for (group_id, obj_id) in fragment_keys:
            info = fragment_info[(group_id, obj_id)]
            track_id = info['track_id']

            moof_path = f"{source_prefix}_moof_g{group_id}_o{obj_id}.bin"
            mdat_path = f"{source_prefix}_mdat_g{group_id}_o{obj_id}_track{track_id}.bin"

            if not os.path.exists(moof_path) or not os.path.exists(mdat_path):
                missing_count += 1
                continue

            # Create temporary combined fragment
            temp_frag = f"tmp/analysis/temp_{label}_g{group_id}_o{obj_id}.mp4"
            with open(temp_frag, 'wb') as out:
                with open(init_path, 'rb') as init:
                    out.write(init.read())
                with open(moof_path, 'rb') as moof:
                    out.write(moof.read())
                with open(mdat_path, 'rb') as mdat:
                    out.write(mdat.read())

            temp_files.append(temp_frag)
            concat_file.write(f"file '{os.path.abspath(temp_frag)}'\n")

    if missing_count > 0:
        print(f"  ⚠️ {label}: {missing_count} fragments missing from disk")

    if not temp_files:
        print(f"  ❌ {label}: No valid fragments to concatenate")
        os.remove(concat_path)
        return False

    # Build full video using concat demuxer
    cmd = [
        'tools/ffmpeg/ffmpeg-7.0.2-amd64-static/ffmpeg',
        '-hide_banner',
        '-f', 'concat',
        '-safe', '0',
        '-i', concat_path,
        '-c', 'copy',
        '-y',
        output_video
    ]

    result = subprocess.run(cmd, capture_output=True, text=True)

    # Cleanup
    os.remove(concat_path)
    for temp_file in temp_files:
        if os.path.exists(temp_file):
            os.remove(temp_file)

    if result.returncode != 0:
        print(f"  ❌ {label}: FFmpeg concat failed: {result.stderr}")
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
        print(f"STDERR: {result.stderr}")
        return None, None, None

    # Parse VMAF
    vmaf_score = None
    if os.path.exists(vmaf_json):
        try:
            with open(vmaf_json, 'r') as f:
                data = json.load(f)
                vmaf_score = data['pooled_metrics']['vmaf']['mean']
        except (json.JSONDecodeError, KeyError) as e:
            print(f"❌ Failed to parse VMAF JSON: {e}")

    # Parse PSNR
    psnr_score = None
    if os.path.exists(psnr_log):
        try:
            with open(psnr_log, 'r') as f:
                # PSNR log format: frame:... mse_avg:... psnr_avg:...
                last_line = f.readlines()[-1]
                if 'psnr_avg:' in last_line:
                    psnr_avg = last_line.split('psnr_avg:')[1].split()[0]
                    psnr_score = float(psnr_avg)
        except (IndexError, ValueError) as e:
            print(f"❌ Failed to parse PSNR log: {e}")

    # Parse SSIM
    ssim_score = None
    if os.path.exists(ssim_log):
        try:
            with open(ssim_log, 'r') as f:
                # SSIM log format: n:... Y:... U:... V:... All:...
                last_line = f.readlines()[-1]
                if 'All:' in last_line:
                    ssim_all = last_line.split('All:')[1].split()[0]
                    ssim_score = float(ssim_all)
        except (IndexError, ValueError) as e:
            print(f"❌ Failed to parse SSIM log: {e}")

    return vmaf_score, psnr_score, ssim_score


def main():
    track_id = 1
    pub_manifest = f"tmp/pub_manifest_track{track_id}.txt"
    sub_manifest = f"tmp/sub/sub_manifest_track{track_id}.txt"
    init_path = f"tmp/init_track{track_id}.mp4"

    if not os.path.exists(pub_manifest) or not os.path.exists(sub_manifest):
        print("❌ Manifest files not found")
        return

    Path("tmp/analysis").mkdir(exist_ok=True)

    # Build full videos from COMMON fragments only (time-aligned)
    pub_full_video = "tmp/analysis/pub_full.mp4"
    sub_full_video = "tmp/analysis/sub_full.mp4"

    success_pub, success_sub, stats = build_full_video_from_common_fragments(
        pub_manifest, sub_manifest, init_path,
        pub_full_video, sub_full_video,
        "tmp/pub", "tmp/sub/sub"
    )

    if not success_pub or not success_sub:
        print("❌ Failed to build videos")
        return

    # Run quality metrics (VMAF, PSNR, SSIM)
    vmaf_log = "tmp/analysis/vmaf_full.json"
    psnr_log = "tmp/analysis/psnr_full.log"
    ssim_log = "tmp/analysis/ssim_full.log"

    vmaf_score, psnr_score, ssim_score = run_quality_metrics(
        sub_full_video, pub_full_video, vmaf_log, psnr_log, ssim_log
    )

    print("\n" + "="*60)
    print("📈 FINAL RESULTS")
    print("="*60)

    if vmaf_score is not None:
        print(f"\n🎯 Video Quality Metrics:")
        print(f"  VMAF:  {vmaf_score:.2f} / 100.00  (perceptual quality)")

        if psnr_score is not None:
            print(f"  PSNR:  {psnr_score:.2f} dB       (pixel-level accuracy)")
        else:
            print(f"  PSNR:  N/A")

        if ssim_score is not None:
            print(f"  SSIM:  {ssim_score:.4f}        (structural similarity)")
        else:
            print(f"  SSIM:  N/A")

        print(f"\n📊 Delivery Metrics:")
        print(f"  Delivery Rate: {100 - stats['loss_rate']:.1f}%")
        print(f"  Packet Loss:   {stats['loss_rate']:.1f}%")

        # Save results
        results = {
            'vmaf_score': vmaf_score,
            'psnr_score': psnr_score,
            'ssim_score': ssim_score,
            'total_sent': stats['total_sent'],
            'total_received': stats['total_received'],
            'common_fragments': stats['common'],
            'lost_fragments': stats['lost'],
            'loss_rate_percent': stats['loss_rate'],
            'delivery_rate_percent': 100 - stats['loss_rate'],
            'pub_video_size': os.path.getsize(pub_full_video),
            'sub_video_size': os.path.getsize(sub_full_video),
        }

        with open('tmp/vmaf_results.json', 'w') as f:
            json.dump(results, f, indent=2)

        print(f"\n📁 Results saved to tmp/vmaf_results.json")
        print(f"📁 PSNR log: {psnr_log}")
        print(f"📁 SSIM log: {ssim_log}")
        print(f"📁 VMAF log: {vmaf_log}")
    else:
        print("❌ Quality metrics analysis failed")


if __name__ == '__main__':
    main()
