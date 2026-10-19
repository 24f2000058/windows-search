// C++ port of flint-fuzzy's hot kernels, for the Rust-vs-C++ comparison.
// Mirrors crates/flint-fuzzy/src/kernels.rs (Pattern, osa_substring,
// subsequence_positions) and `flint-bench kernels` loop exactly; hit counts
// must match the Rust run or the comparison is invalid.
//
// Build: g++ -O3 -std=c++17 kernels.cpp -o kernels   (or clang++)
// Run:   ./kernels names.txt
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <string>
#include <utility>
#include <vector>

static std::vector<uint32_t> decode_utf8(const std::string& s) {
    std::vector<uint32_t> out;
    out.reserve(s.size());
    for (size_t i = 0; i < s.size();) {
        unsigned char c = s[i];
        uint32_t cp;
        int n;
        if (c < 0x80) { cp = c; n = 1; }
        else if ((c >> 5) == 6) { cp = c & 0x1f; n = 2; }
        else if ((c >> 4) == 14) { cp = c & 0x0f; n = 3; }
        else { cp = c & 0x07; n = 4; }
        for (int k = 1; k < n && i + k < s.size(); k++) cp = (cp << 6) | (s[i + k] & 0x3f);
        out.push_back(cp);
        i += n;
    }
    return out;
}

struct Pattern {
    std::vector<uint32_t> chars;
    uint64_t ascii_peq[128] = {0};
    std::vector<std::pair<uint32_t, uint64_t>> other;
    explicit Pattern(const std::vector<uint32_t>& all) {
        chars.assign(all.begin(), all.begin() + std::min<size_t>(all.size(), 64));
        for (size_t i = 0; i < chars.size(); i++) {
            uint32_t c = chars[i];
            if (c < 128) { ascii_peq[c] |= 1ull << i; continue; }
            bool found = false;
            for (auto& e : other) if (e.first == c) { e.second |= 1ull << i; found = true; break; }
            if (!found) other.push_back({c, 1ull << i});
        }
    }
    inline uint64_t peq(uint32_t c) const {
        if (c < 128) return ascii_peq[c];
        for (auto& e : other) if (e.first == c) return e.second;
        return 0;
    }
    uint64_t word_mask() const { return chars.size() == 64 ? ~0ull : (1ull << chars.size()) - 1; }
};

// Hyyrö bit-vector OSA distance, approximate-substring mode.
static bool osa_substring(const Pattern& p, const std::vector<uint32_t>& t, uint32_t max_k) {
    size_t m = p.chars.size();
    if (m == 0) return true;
    uint64_t mask = p.word_mask(), high = 1ull << (m - 1);
    uint64_t vp = mask, vn = 0, d0_prev = 0, eq_prev = 0;
    uint32_t score = (uint32_t)m, best = score;
    size_t best_end = 0, j = 0;  // tracked like the Rust kernel (end of best match)
    for (uint32_t c : t) {
        j++;
        uint64_t eq = p.peq(c);
        uint64_t tr = (((~d0_prev) & eq) << 1) & eq_prev;
        uint64_t d0 = (((eq & vp) + vp) ^ vp) | eq | vn | tr;
        uint64_t hp = vn | ~(d0 | vp);
        uint64_t hn = vp & d0;
        if (hp & high) score++; else if (hn & high) score--;
        hp <<= 1; hn <<= 1;
        vp = (hn | ~(d0 | hp)) & mask;
        vn = hp & d0 & mask;
        d0_prev = d0; eq_prev = eq;
        if (score < best) { best = score; best_end = j; if (score == 0) break; }
    }
    volatile size_t sink = best_end; (void)sink;
    return best <= max_k;
}

// Forward pass + backward tightening, returning positions (as Rust does).
static bool subsequence_positions(const std::vector<uint32_t>& p, const std::vector<uint32_t>& t, std::vector<size_t>& pos) {
    if (p.empty()) return true;
    size_t pi = 0, end = SIZE_MAX;
    for (size_t j = 0; j < t.size(); j++) {
        if (t[j] == p[pi] && ++pi == p.size()) { end = j; break; }
    }
    if (end == SIZE_MAX) return false;
    pos.assign(p.size(), 0);
    pi = p.size();
    size_t j = end + 1;
    while (pi > 0) {
        j--;
        if (t[j] == p[pi - 1]) pos[--pi] = j;
    }
    return true;
}

static uint32_t max_edits_normal(size_t len) { size_t k = len / 4; return (uint32_t)(k < 3 ? k : 3); }

int main(int argc, char** argv) {
    const char* path = argc > 1 ? argv[1] : "names.txt";
    std::vector<std::vector<uint32_t>> names, queries;
    std::string line;
    { std::ifstream f(path); while (std::getline(f, line)) names.push_back(decode_utf8(line)); }
    { std::ifstream f(std::string(path) + ".queries"); while (std::getline(f, line)) queries.push_back(decode_utf8(line)); }
    auto t0 = std::chrono::steady_clock::now();
    uint64_t osa_hits = 0, sub_hits = 0;
    std::vector<size_t> pos;
    for (auto& q : queries) {
        Pattern p(q);
        uint32_t k = max_edits_normal(q.size());
        for (auto& n : names) {
            if (osa_substring(p, n, k)) osa_hits++;
            if (subsequence_positions(q, n, pos)) sub_hits++;
        }
    }
    double el = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double pairs = (double)queries.size() * (double)names.size();
    std::printf("c++ kernels: %zu queries x %zu names: %.3fs, %.1f ns/name-pair, osa_hits=%llu sub_hits=%llu\n",
                queries.size(), names.size(), el, el * 1e9 / pairs, (unsigned long long)osa_hits, (unsigned long long)sub_hits);
}
