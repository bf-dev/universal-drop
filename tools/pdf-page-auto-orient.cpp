#include <opencv2/core.hpp>
#include <opencv2/imgcodecs.hpp>
#include <opencv2/imgproc.hpp>

#include <algorithm>
#include <cmath>
#include <cctype>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

namespace {

constexpr int kMaxAnalysisDim = 1400;
constexpr double kMinForegroundRatio = 0.0015;
constexpr int kMinForegroundPixels = 900;
constexpr double kUpsideDownScoreThreshold = -0.24;
constexpr double kSidewaysProjectionRatio = 1.25;
constexpr double kSidewaysMinDelta = 0.35;
constexpr double kSidewaysUprightDelta = 0.55;
constexpr double kLongRuleMinLengthRatio = 0.55;
constexpr double kLongRuleMaxThicknessRatio = 0.018;
constexpr double kArtifactHeavyRemovedRatio = 0.07;
constexpr double kArtifactHeavyRawToCleanRatio = 2.2;
constexpr int kMinUpsideDownTextLikeComponents = 80;

struct Analysis {
    int rotation = 0;
    double confidence = 0.0;
    double raw_foreground_ratio = 0.0;
    double removed_foreground_ratio = 0.0;
    double foreground_ratio = 0.0;
    double horizontal_score = 0.0;
    double vertical_score = 0.0;
    double upright_score = 0.0;
    int kept_components = 0;
    int text_like_components = 0;
    int long_rule_components = 0;
    std::string reason = "not_needed";
};

struct MaskResult {
    cv::Mat mask;
    double raw_foreground_ratio = 0.0;
    double removed_foreground_ratio = 0.0;
    int kept_components = 0;
    int text_like_components = 0;
    int long_rule_components = 0;
};

std::string json_escape(const std::string &value) {
    std::ostringstream out;
    for (char ch : value) {
        switch (ch) {
        case '\\': out << "\\\\"; break;
        case '"': out << "\\\""; break;
        case '\n': out << "\\n"; break;
        case '\r': out << "\\r"; break;
        case '\t': out << "\\t"; break;
        default: out << ch; break;
        }
    }
    return out.str();
}

cv::Mat resize_for_analysis(const cv::Mat &gray) {
    int max_dim = std::max(gray.cols, gray.rows);
    if (max_dim <= kMaxAnalysisDim) {
        return gray.clone();
    }
    double scale = static_cast<double>(kMaxAnalysisDim) / static_cast<double>(max_dim);
    cv::Mat resized;
    cv::resize(gray, resized, cv::Size(), scale, scale, cv::INTER_AREA);
    return resized;
}

bool is_long_rule_component(int width, int height, int cols, int rows) {
    int max_horizontal_thickness =
        std::max(3, static_cast<int>(std::round(rows * kLongRuleMaxThicknessRatio)));
    int max_vertical_thickness =
        std::max(3, static_cast<int>(std::round(cols * kLongRuleMaxThicknessRatio)));

    bool horizontal_rule =
        width > cols * kLongRuleMinLengthRatio && height <= max_horizontal_thickness;
    bool vertical_rule =
        height > rows * kLongRuleMinLengthRatio && width <= max_vertical_thickness;
    return horizontal_rule || vertical_rule;
}

bool is_text_like_component(int area, int width, int height, int total_area, int cols, int rows) {
    int min_area = std::max(4, total_area / 350000);
    int max_area = std::max(60, total_area / 900);
    if (area < min_area || area > max_area) {
        return false;
    }
    if (width < 2 || height < 3) {
        return false;
    }
    if (width > cols * 0.25 || height > rows * 0.12) {
        return false;
    }
    double aspect = static_cast<double>(width) / static_cast<double>(std::max(1, height));
    return aspect >= 0.05 && aspect <= 18.0;
}

MaskResult foreground_mask(const cv::Mat &gray) {
    cv::Mat blurred;
    cv::GaussianBlur(gray, blurred, cv::Size(3, 3), 0.0);

    cv::Mat mask;
    cv::threshold(blurred, mask, 0, 255, cv::THRESH_BINARY_INV | cv::THRESH_OTSU);

    MaskResult result;
    int total_area = mask.rows * mask.cols;
    int raw_foreground = cv::countNonZero(mask);
    result.raw_foreground_ratio =
        static_cast<double>(raw_foreground) / static_cast<double>(std::max(1, total_area));

    cv::Mat labels, stats, centroids;
    int components = cv::connectedComponentsWithStats(mask, labels, stats, centroids, 8, CV_32S);
    cv::Mat cleaned = cv::Mat::zeros(mask.size(), CV_8U);
    int min_area = std::max(3, total_area / 250000);
    int max_area = std::max(200, total_area / 5);

    for (int label = 1; label < components; ++label) {
        int area = stats.at<int>(label, cv::CC_STAT_AREA);
        int width = stats.at<int>(label, cv::CC_STAT_WIDTH);
        int height = stats.at<int>(label, cv::CC_STAT_HEIGHT);
        if (area < min_area || area > max_area) {
            continue;
        }
        // Drop page-edge scanner shadows or giant borders. Real text/logos/stamps remain.
        if (width > mask.cols * 0.96 && height > mask.rows * 0.03) {
            continue;
        }
        if (height > mask.rows * 0.96 && width > mask.cols * 0.03) {
            continue;
        }
        // Ruled notebook paper, table borders, and pharmacy-slip separators are
        // strong horizontal/vertical projection signals but not page-orientation
        // evidence. Keeping them made bottom-heavy handwritten pages look like
        // upside-down printed documents, so drop long thin rules before scoring.
        if (is_long_rule_component(width, height, mask.cols, mask.rows)) {
            result.long_rule_components += 1;
            continue;
        }
        result.kept_components += 1;
        if (is_text_like_component(area, width, height, total_area, mask.cols, mask.rows)) {
            result.text_like_components += 1;
        }
        cleaned.setTo(255, labels == label);
    }
    result.mask = cleaned;
    int cleaned_foreground = cv::countNonZero(cleaned);
    result.removed_foreground_ratio =
        static_cast<double>(std::max(0, raw_foreground - cleaned_foreground)) /
        static_cast<double>(std::max(1, total_area));
    return result;
}

cv::Mat rotate_quadrant(const cv::Mat &image, int degrees) {
    cv::Mat rotated;
    int normalized = ((degrees % 360) + 360) % 360;
    switch (normalized) {
    case 0:
        return image.clone();
    case 90:
        cv::rotate(image, rotated, cv::ROTATE_90_CLOCKWISE);
        return rotated;
    case 180:
        cv::rotate(image, rotated, cv::ROTATE_180);
        return rotated;
    case 270:
        cv::rotate(image, rotated, cv::ROTATE_90_COUNTERCLOCKWISE);
        return rotated;
    default:
        return image.clone();
    }
}

double projection_score(const cv::Mat &mask, bool rows) {
    cv::Mat projected;
    int reduce_dim = rows ? 1 : 0;
    cv::reduce(mask, projected, reduce_dim, cv::REDUCE_SUM, CV_64F);

    const int n = rows ? projected.rows : projected.cols;
    if (n <= 0) {
        return 0.0;
    }

    std::vector<double> values;
    values.reserve(static_cast<size_t>(n));
    for (int i = 0; i < n; ++i) {
        double raw = rows ? projected.at<double>(i, 0) : projected.at<double>(0, i);
        values.push_back(raw / 255.0);
    }

    double mean = 0.0;
    for (double value : values) {
        mean += value;
    }
    mean /= static_cast<double>(values.size());
    if (mean < 0.5) {
        return 0.0;
    }

    double variance = 0.0;
    for (double value : values) {
        double diff = value - mean;
        variance += diff * diff;
    }
    variance /= static_cast<double>(values.size());
    double stdev = std::sqrt(variance);

    double active_threshold = std::max(3.0, mean + stdev * 0.30);
    int active_runs = 0;
    bool in_run = false;
    for (double value : values) {
        bool active = value > active_threshold;
        if (active && !in_run) {
            active_runs += 1;
        }
        in_run = active;
    }

    double peak_factor = std::sqrt(static_cast<double>(active_runs) + 1.0);
    return (stdev / (mean + 1.0)) * peak_factor;
}

double content_upright_score(const cv::Mat &mask) {
    std::vector<cv::Point> points;
    cv::findNonZero(mask, points);
    if (points.empty()) {
        return 0.0;
    }

    int h = mask.rows;
    int top_start = 0;
    int top_end = static_cast<int>(std::round(h * 0.35));
    int bottom_start = static_cast<int>(std::round(h * 0.65));
    int top_count = cv::countNonZero(mask(cv::Range(top_start, top_end), cv::Range::all()));
    int bottom_count = cv::countNonZero(mask(cv::Range(bottom_start, h), cv::Range::all()));
    double density_score = static_cast<double>(top_count - bottom_count) /
                           static_cast<double>(top_count + bottom_count + 1);

    double sum_y = 0.0;
    for (const auto &point : points) {
        sum_y += static_cast<double>(point.y);
    }
    double centroid_y = sum_y / static_cast<double>(points.size());
    double centroid_score = (0.5 - (centroid_y / std::max(1, h - 1))) * 2.0;

    return 0.75 * density_score + 0.25 * centroid_score;
}

Analysis analyze_orientation(const cv::Mat &gray) {
    // Deliberately conservative: this helper only returns whole-page quadrant
    // rotations (0/90/180/270). It never deskews or rotates by small angles,
    // so scans tilted under the requested 45-50 degree threshold stay untouched.
    Analysis analysis;
    cv::Mat small = resize_for_analysis(gray);
    MaskResult mask_result = foreground_mask(small);
    cv::Mat mask = mask_result.mask;
    int foreground = cv::countNonZero(mask);
    int area = mask.rows * mask.cols;
    analysis.raw_foreground_ratio = mask_result.raw_foreground_ratio;
    analysis.removed_foreground_ratio = mask_result.removed_foreground_ratio;
    analysis.foreground_ratio = static_cast<double>(foreground) / static_cast<double>(area);
    analysis.kept_components = mask_result.kept_components;
    analysis.text_like_components = mask_result.text_like_components;
    analysis.long_rule_components = mask_result.long_rule_components;

    if (foreground < kMinForegroundPixels || analysis.foreground_ratio < kMinForegroundRatio) {
        analysis.reason = "low_foreground_skip";
        return analysis;
    }

    analysis.horizontal_score = projection_score(mask, true);
    analysis.vertical_score = projection_score(mask, false);
    analysis.upright_score = content_upright_score(mask);

    bool sideways = analysis.vertical_score > analysis.horizontal_score * kSidewaysProjectionRatio &&
                    (analysis.vertical_score - analysis.horizontal_score) > kSidewaysMinDelta;

    if (sideways) {
        cv::Mat cw = rotate_quadrant(mask, 90);
        cv::Mat ccw = rotate_quadrant(mask, 270);
        double cw_score = content_upright_score(cw);
        double ccw_score = content_upright_score(ccw);
        double delta = std::abs(cw_score - ccw_score);
        if (delta >= kSidewaysUprightDelta) {
            analysis.rotation = cw_score >= ccw_score ? 90 : 270;
            analysis.confidence = std::min(1.0, delta);
            analysis.reason = "sideways_text_lines";
            return analysis;
        }
        analysis.reason = "sideways_low_upright_confidence_skip";
        analysis.confidence = delta;
        return analysis;
    }

    if (analysis.upright_score <= kUpsideDownScoreThreshold) {
        bool artifact_heavy =
            analysis.removed_foreground_ratio >= kArtifactHeavyRemovedRatio &&
            analysis.raw_foreground_ratio >=
                analysis.foreground_ratio * kArtifactHeavyRawToCleanRatio;
        if (artifact_heavy) {
            analysis.confidence = std::min(1.0, -analysis.upright_score);
            analysis.reason = "artifact_heavy_upside_down_skip";
            return analysis;
        }

        if (analysis.text_like_components < kMinUpsideDownTextLikeComponents) {
            analysis.confidence = std::min(1.0, -analysis.upright_score);
            analysis.reason = "low_text_evidence_upside_down_skip";
            return analysis;
        }

        analysis.rotation = 180;
        analysis.confidence = std::min(1.0, -analysis.upright_score);
        analysis.reason = "bottom_heavy_upside_down";
        return analysis;
    }

    analysis.confidence = std::max(0.0, analysis.upright_score);
    analysis.reason = "upright_or_low_confidence";
    return analysis;
}

bool write_image_like_input(const std::string &output_path, const cv::Mat &image) {
    std::vector<int> params;
    std::string lower = output_path;
    std::transform(lower.begin(), lower.end(), lower.begin(), [](unsigned char c) {
        return static_cast<char>(std::tolower(c));
    });
    bool jpeg = lower.size() >= 4 && lower.rfind(".jpg") == lower.size() - 4;
    bool jpeg_long = lower.size() >= 5 && lower.rfind(".jpeg") == lower.size() - 5;
    if (jpeg || jpeg_long) {
        params.push_back(cv::IMWRITE_JPEG_QUALITY);
        params.push_back(95);
    }
    return cv::imwrite(output_path, image, params);
}

} // namespace

int main(int argc, char **argv) {
    if (argc < 3) {
        std::cerr << "usage: pdf-page-auto-orient <input-image> <output-image>\n";
        return 64;
    }

    std::string input_path = argv[1];
    std::string output_path = argv[2];

    cv::Mat image = cv::imread(input_path, cv::IMREAD_COLOR);
    if (image.empty()) {
        std::cerr << "failed to read image: " << input_path << "\n";
        return 66;
    }

    cv::Mat gray;
    cv::cvtColor(image, gray, cv::COLOR_BGR2GRAY);
    Analysis analysis = analyze_orientation(gray);

    cv::Mat output = rotate_quadrant(image, analysis.rotation);
    if (!write_image_like_input(output_path, output)) {
        std::cerr << "failed to write image: " << output_path << "\n";
        return 74;
    }

    std::cout << "{"
              << "\"input\":\"" << json_escape(input_path) << "\","
              << "\"output\":\"" << json_escape(output_path) << "\","
              << "\"rotation\":" << analysis.rotation << ","
              << "\"confidence\":" << analysis.confidence << ","
              << "\"raw_foreground_ratio\":" << analysis.raw_foreground_ratio << ","
              << "\"removed_foreground_ratio\":" << analysis.removed_foreground_ratio << ","
              << "\"foreground_ratio\":" << analysis.foreground_ratio << ","
              << "\"horizontal_score\":" << analysis.horizontal_score << ","
              << "\"vertical_score\":" << analysis.vertical_score << ","
              << "\"upright_score\":" << analysis.upright_score << ","
              << "\"kept_components\":" << analysis.kept_components << ","
              << "\"text_like_components\":" << analysis.text_like_components << ","
              << "\"long_rule_components\":" << analysis.long_rule_components << ","
              << "\"reason\":\"" << json_escape(analysis.reason) << "\""
              << "}\n";
    return 0;
}
