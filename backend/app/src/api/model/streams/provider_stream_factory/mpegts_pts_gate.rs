//! Drops the replayed prefix of a reopened live MPEG-TS episode.
//!
//! Some panels answer a channel with a body that ends after a few megabytes and serve the next
//! request from a few seconds **behind** live edge, racing forward until it catches up and closing
//! again. Splicing such episodes back to back replays content the viewer already received and
//! throws the presentation timestamps backwards at every boundary. Players treat that backward
//! jump as clock jitter and start discarding pictures, which the viewer sees as a frozen frame.
//!
//! The gate keeps the last presentation timestamp emitted for every PES pid. When a new episode
//! begins, packets of a pid are held back until that pid's timestamps have advanced past what was
//! already delivered; from there on everything passes through. Because the provider replays the
//! original packets, the embedded PCR travels with the same timeline, so dropping the overlapped
//! packets also removes the backward PCR jump — no timestamp rewriting is needed.
//!
//! Streams that are not transport stream at all (no `0x47` sync, no PES) never lock the gate and
//! pass through unchanged, so the wrapper keeps working for any provider.

use std::collections::HashMap;

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
/// Backward tolerance: a PES pid may reopen slightly behind its last emitted timestamp without
/// being considered overlapped (encoder jitter between episodes).
const REOPEN_BACKWARD_TOLERANCE_TICKS: u64 = 9_000; // 100 ms in 90 kHz ticks
const PTS_MODULUS: u64 = 1 << 33;
const PTS_HALF_RANGE: u64 = 1 << 32;

#[derive(Default)]
struct PidGate {
    /// Last presentation timestamp emitted for this pid, in 90 kHz ticks.
    last_emitted_pts: u64,
    /// Whether this episode's packets for this pid may pass through yet.
    open: bool,
}

/// Stateful per-pid overlap filter for one live MPEG-TS response.
#[derive(Default)]
pub(super) struct MpegTsPtsGate {
    buffer: Vec<u8>,
    /// Pids that carried a PES presentation timestamp at least once.
    pids: HashMap<u16, PidGate>,
    /// Set once a packet yielded a presentation timestamp; until then everything passes through.
    locked: bool,
}

impl MpegTsPtsGate {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Starts gating the next episode: each pid must again prove its timestamps moved forward.
    pub(super) fn begin_new_episode(&mut self) {
        for gate in self.pids.values_mut() {
            gate.open = false;
        }
    }

    /// Feeds one upstream chunk and appends the bytes that may still be delivered.
    pub(super) fn push_chunk(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.buffer.extend_from_slice(chunk);
        while !self.buffer.is_empty() {
            if self.buffer[0] != SYNC_BYTE {
                // Before the first presentation timestamp there is no proof this is a transport
                // stream, so unknown bytes are passed through instead of silently dropped.
                let byte = self.buffer.remove(0);
                if !self.locked {
                    out.push(byte);
                }
                continue;
            }
            if self.buffer.len() < TS_PACKET_SIZE {
                // A partial packet stays buffered until its remainder arrives.
                break;
            }
            let packet: Vec<u8> = self.buffer.drain(..TS_PACKET_SIZE).collect();
            if self.packet_may_pass(&packet) {
                out.extend_from_slice(&packet);
            }
        }
    }

    /// Whether one transport packet belongs after everything already emitted.
    fn packet_may_pass(&mut self, packet: &[u8]) -> bool {
        let pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
        let payload_unit_start = packet[1] & 0x40 != 0;
        let adaptation_field_control = (packet[3] >> 4) & 0x3;
        let mut payload_start = 4;
        if adaptation_field_control & 0b10 != 0 {
            let Some(&adaptation_length) = packet.get(4) else { return !self.locked };
            payload_start = 5 + adaptation_length as usize;
        }
        if adaptation_field_control & 0b01 == 0 {
            // No payload: keep-alive/PCR-only packets follow their pid's verdict.
            return self.pid_may_pass(pid);
        }
        let pts = if payload_unit_start { parse_pes_pts(packet, payload_start) } else { None };
        match pts {
            Some(pts) => {
                self.locked = true;
                let gate = self.pids.entry(pid).or_insert(PidGate { last_emitted_pts: pts, open: true });
                if gate.open && pts_is_behind(pts, gate.last_emitted_pts) {
                    // A mid-body rewind from the provider is the same replay as an episode
                    // boundary: hold this pid back until its timestamps catch up again.
                    gate.open = false;
                }
                if gate.open || !pts_is_behind(pts, gate.last_emitted_pts) {
                    gate.open = true;
                    gate.last_emitted_pts = pts;
                    true
                } else {
                    false
                }
            }
            None => {
                // Continuation fragments of an open PES, or PSI/NULL pids that never carry a
                // timestamp. Pids the gate knows about follow their verdict; unknown pids pass.
                self.pid_may_pass(pid)
            }
        }
    }

    fn pid_may_pass(&self, pid: u16) -> bool {
        self.pids.get(&pid).is_none_or(|gate| gate.open)
    }
}

/// Extracts the presentation timestamp of a PES packet that starts at `payload_start`.
fn parse_pes_pts(packet: &[u8], payload_start: usize) -> Option<u64> {
    let start = packet.get(payload_start..payload_start + 6)?;
    if start[0] != 0x00 || start[1] != 0x00 || start[2] != 0x01 {
        return None;
    }
    let stream_id = start[3];
    let is_media = matches!(stream_id, 0xC0..=0xDF | 0xE0..=0xEF);
    if !is_media {
        return None;
    }
    let header_data_length = *packet.get(payload_start + 8)?;
    if header_data_length < 5 {
        return None;
    }
    let flags = *packet.get(payload_start + 7)? >> 6;
    if flags == 0 {
        return None;
    }
    let t = packet.get(payload_start + 9..payload_start + 14)?;
    Some(
        (((t[0] >> 1) & 0x07) as u64) << 30
            | (((t[1] as u64) << 7 | (t[2] >> 1) as u64)) << 15
            | ((t[3] as u64) << 7 | (t[4] >> 1) as u64),
    )
}

/// Whether `pts` sits meaningfully behind `last` on the wrapping 33-bit clock.
fn pts_is_behind(pts: u64, last: u64) -> bool {
    let forward = (pts + PTS_MODULUS - last % PTS_MODULUS) % PTS_MODULUS;
    forward > PTS_HALF_RANGE && (PTS_MODULUS - forward) > REOPEN_BACKWARD_TOLERANCE_TICKS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal transport packet carrying a PES with the given presentation timestamp.
    fn packet(pid: u16, pts: u64) -> Vec<u8> {
        let mut packet = vec![0u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) & 0x1F) as u8; // payload unit start
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10; // payload only
        let pts33 = pts % PTS_MODULUS;
        packet[4..10].copy_from_slice(&[
            0x00,
            0x00,
            0x01,
            0xE0,
            0x00,
            0x00, // PES: video stream id, no length (multipart)
        ]);
        packet[10] = 0x80; // flags marker '10'
        packet[11] = 0x80; // PTS_DTS flags: PTS only
        packet[12] = 5; // header data length
        packet[13] = 0x21 | (((pts33 >> 30) & 0x07) as u8) << 1;
        packet[14] = ((pts33 >> 22) & 0xFF) as u8;
        packet[15] = 0x01 | ((((pts33 >> 15) & 0x7F) as u8) << 1);
        packet[16] = ((pts33 >> 7) & 0xFF) as u8;
        packet[17] = 0x01 | ((pts33 & 0x7F) as u8) << 1;
        packet
    }

    fn pts_seconds(seconds: u64) -> u64 {
        seconds * 90_000
    }

    #[test]
    fn overlapped_prefix_is_dropped_and_continuation_passes() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();

        // Episode 1 delivers up to 60 s.
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(60)), &mut out);
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE, "the first episode passes through");

        // Episode 2 replays from 50 s: the 50 s packet must be dropped, 60 s onwards passes.
        gate.begin_new_episode();
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert!(out.is_empty(), "a packet the viewer already received is dropped");
        gate.push_chunk(&packet(256, pts_seconds(60)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the episode resumes at the last emitted timestamp");
    }

    #[test]
    fn a_small_backward_jitter_still_opens_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(100)), &mut out);

        gate.begin_new_episode();
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(100) - 4_500), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "50 ms of encoder jitter is not an overlap");
    }

    #[test]
    fn audio_and_video_gate_independently() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(30)), &mut out);
        gate.push_chunk(&packet(257, pts_seconds(30)), &mut out);

        gate.begin_new_episode();
        out.clear();
        // Video still overlapped, audio already past: only audio may pass.
        gate.push_chunk(&packet(256, pts_seconds(20)), &mut out);
        gate.push_chunk(&packet(257, pts_seconds(31)), &mut out);
        assert!(out.windows(TS_PACKET_SIZE).any(|window| window[2] == (257 & 0xFF) as u8), "audio continues");
        assert!(!out.windows(TS_PACKET_SIZE).any(|window| window[2] == (256 & 0xFF) as u8), "video stays held");
    }

    #[test]
    fn a_mid_body_rewind_re_arms_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(40)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);

        // The provider rewinds inside the same body: the replay is dropped and delivery resumes
        // only once the timestamps are back at the last delivered point.
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(40)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(45)), &mut out);
        assert!(out.is_empty(), "a mid-body rewind is treated as replay");
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "delivery resumes at the last delivered timestamp");
    }

    #[test]
    fn a_stream_without_transport_sync_never_locks_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(b"first-episode", &mut out);
        gate.begin_new_episode();
        gate.push_chunk(b"second-episode", &mut out);
        assert_eq!(out, b"first-episodesecond-episode", "non-TS bytes pass through untouched");
    }

    #[test]
    fn partial_packets_are_reassembled_across_chunks() {
        let mut gate = MpegTsPtsGate::new();
        let packet = packet(256, pts_seconds(1));
        let mut out = Vec::new();
        gate.push_chunk(&packet[..100], &mut out);
        assert!(out.is_empty(), "an incomplete packet is held, never emitted early");
        gate.push_chunk(&packet[100..], &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the packet is emitted once complete");
    }
}