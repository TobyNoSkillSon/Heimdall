//! [chrome] Naming a voice from the transcript window (night 1022, stage 3; "double-click an
//! unidentified 'Speaker 2' to name it, which adds it as a known speaker"). A coded label is renamed directly; a
//! "Speaker N" resolves to its call track and the code that track's voice has (`Pipeline::rename_voice`).
use super::*;

/// "Speaker 2" → 2 (the call diariser's anonymous names; "Speaker ?" has no voice to name).
pub(super) fn unidentified_speaker(speaker: &str) -> Option<i32> {
    speaker.strip_prefix("Speaker ")?.parse::<i32>().ok().filter(|n| (1..=8).contains(n))
}

impl Pipeline {
    /// The voice a transcript label names: speaker N of the line's own call session, or of the open call whose name
    /// the label shows when the line has no track.
    pub(super) fn transcript_track(&self, line_id: u64, source_label: &str, speaker: &str) -> Result<TrackId> {
        let n = unidentified_speaker(speaker).with_context(|| format!("{speaker} is not an unidentified speaker"))?;
        if let Some((_, Some(t))) = self.labels.get(&line_id) {
            return Ok(TrackId { speaker: n, ..t.clone() });
        }
        let label_of = |id: &str| self.sources.get(id).map_or_else(|| display_name_from_id(id), display_name);
        self.discord
            .iter()
            .find(|(id, _)| label_of(id) == source_label)
            .map(|(id, d)| TrackId { source: id.clone(), session: d.session, speaker: n })
            .context("that call is no longer open")
    }

    pub(super) fn name_transcript_speaker(&mut self, line_id: u64, source: &str, speaker: &str, name: &str) -> Result<()> {
        let name = name.trim().to_string();
        ensure!(!name.is_empty(), "empty name");
        // A coded voice ("Speaker Fri 23 Oct 21h #3") or a name: renamed everywhere, past lines included.
        if unidentified_speaker(speaker).is_none() {
            self.rename_voice(speaker, &name)?;
            return Ok(());
        }
        // "Speaker N": the voice the call has coded for that track, once it spoke clearly enough.
        let track = self.transcript_track(line_id, source, speaker)?;
        let code = self.speaker_names.get(&track).cloned().with_context(|| format!("{speaker} has not spoken clearly enough yet to be remembered"))?;
        // Only the coded voice behind this track: a name it already has is renamed from Speakers, never from here.
        ensure!(self.load_store()?.voices.contains_key(&code), "{speaker} is {code} now; rename them from Speakers");
        self.rename_voice(&code, &name)?;
        Ok(())
    }
}
