    // Heimdall: ranked neighbor heap instead of rescanning/copying the complete
    // word after every merge. Rank ties keep the original leftmost occurrence.
    // Heimdall: words recur across transcripts; BPE is a pure function of the word for
    // this tokenizer instance, so memoize it (bounded, cleared when full).
    final class HeimdallWordCache: @unchecked Sendable { let lock = NSLock(); var entries: [String: [String]] = [:] }
    private let heimdallCacheBox = HeimdallWordCache()
    func bpe(token: String) -> [String] {
        heimdallCacheBox.lock.lock()
        let hit = heimdallCacheBox.entries[token]
        heimdallCacheBox.lock.unlock()
        if let hit { return hit }
        let result = heimdallMerge(token: token)
        heimdallCacheBox.lock.lock()
        if heimdallCacheBox.entries.count >= 200_000 { heimdallCacheBox.entries.removeAll(keepingCapacity: true) }
        heimdallCacheBox.entries[token] = result
        heimdallCacheBox.lock.unlock()
        return result
    }
    private func heimdallMerge(token: String) -> [String] {
        // Hugging Face BPE starts from Unicode scalars, not Swift grapheme clusters.
        var words = token.unicodeScalars.map { String($0) }
        guard words.count > 1 else { return [token] }
        struct Candidate {
            let rank: Int
            let left: Int
            let right: Int
            let leftVersion: Int
            let rightVersion: Int
        }
        var previous = (0..<words.count).map { $0 - 1 }
        var next = (0..<words.count).map { $0 + 1 < words.count ? $0 + 1 : -1 }
        var version = [Int](repeating: 0, count: words.count)
        var alive = [Bool](repeating: true, count: words.count)
        var heap: [Candidate] = []
        heap.reserveCapacity(words.count * 2)
        func less(_ a: Candidate, _ b: Candidate) -> Bool {
            a.rank < b.rank || (a.rank == b.rank && a.left < b.left)
        }
        func push(_ i: Int) {
            guard i >= 0, alive[i], next[i] >= 0 else { return }
            let j = next[i]
            guard let rank = bpeRanks[BytePair(words[i], words[j])] else { return }
            heap.append(Candidate(rank: rank, left: i, right: j, leftVersion: version[i], rightVersion: version[j]))
            var at = heap.count - 1
            while at > 0 {
                let parent = (at - 1) / 2
                guard less(heap[at], heap[parent]) else { break }
                heap.swapAt(at, parent); at = parent
            }
        }
        func pop() -> Candidate? {
            guard !heap.isEmpty else { return nil }
            let answer = heap[0], last = heap.removeLast()
            if !heap.isEmpty {
                heap[0] = last
                var at = 0
                while at * 2 + 1 < heap.count {
                    var child = at * 2 + 1
                    if child + 1 < heap.count && less(heap[child+1], heap[child]) { child += 1 }
                    guard less(heap[child], heap[at]) else { break }
                    heap.swapAt(at, child); at = child
                }
            }
            return answer
        }
        for i in 0..<words.count-1 { push(i) }
        while let merge = pop() {
            let (i, j) = (merge.left, merge.right)
            guard alive[i], alive[j], next[i] == j,
                  version[i] == merge.leftVersion, version[j] == merge.rightVersion else { continue }
            words[i] += words[j]
            version[i] += 1; alive[j] = false; next[i] = next[j]
            if next[j] >= 0 { previous[next[j]] = i }
            push(previous[i]); push(i)
        }
        var result: [String] = [], at = 0
        while at >= 0 { result.append(words[at]); at = next[at] }
        // A space-joined intermediate corrupts combining scalars and ZWJ: Swift
        // treats a separator followed by those scalars as one grapheme cluster.
        return result
    }
