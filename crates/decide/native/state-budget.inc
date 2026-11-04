    // Heimdall serializes the critical current utterance before newest-first
    // history. A full question/options head+tail is already reserved in n_room.
    // Count with the actual rendered/escaped model tokenizer, not characters.
    if (state.is_string()) {
        const auto text = state.get<std::string>();
        if (text.rfind("{\"utterance\":", 0) == 0) {
            const auto split = text.find(",\"recent_turns\":");
            if (split == std::string::npos || text.rfind("{\"utterance\":", 0) != 0) {
                throw std::invalid_argument("Heimdall state must put current utterance before history");
            }
            const auto critical = text.substr(0, split);
            const auto rendered = render(critical, questions, question, variant, files.size(), is_audio);
            size_t needed = 0;
            for (const auto & part : string_split(rendered, D1OMNI_SEP)) {
                if (string_starts_with(part, D1OMNI_MARK_STATE)) {
                    needed = common_tokenize(vocab, part.substr(D1OMNI_MARK_STATE.size()), false, true).size();
                }
            }
            // Two boundary tokens cover token merges at the history separator.
            if (needed == 0 || needed + 2 > n_room) {
                throw std::invalid_argument("Current utterance and question exceed d1 token budget; decision refused");
            }
        }
    }
