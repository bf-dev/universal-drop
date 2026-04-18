#include <opencv2/core.hpp>
#include <opencv2/imgcodecs.hpp>
#include <opencv2/imgproc.hpp>

#include <algorithm>
#include <array>
#include <cmath>
#include <cctype>
#include <iostream>
#include <limits>
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
constexpr int kMinLineModelTextComponents = 60;
constexpr int kMinLineModelLines = 4;
constexpr double kLineModelUprightWeight = 0.45;
constexpr double kLineModelUpsideDownMargin = 0.18;
constexpr double kLineModelSidewaysMargin = 0.50;
constexpr double kLineModelSidewaysLayoutRatio = 1.18;
constexpr double kLineModelContradictionMargin = 0.22;
constexpr std::array<int, 4> kCandidateRotations = {0, 90, 180, 270};

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
    int line_model_best_rotation = 0;
    int line_model_lines = 0;
    int line_model_components = 0;
    double line_model_best_score = 0.0;
    double line_model_score_margin = 0.0;
    std::array<double, 4> line_model_scores = {0.0, 0.0, 0.0, 0.0};
    std::array<double, 4> line_model_layout_scores = {0.0, 0.0, 0.0, 0.0};
    std::array<double, 4> line_model_upright_scores = {0.0, 0.0, 0.0, 0.0};
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

struct TextComponent {
    int x = 0;
    int y = 0;
    int width = 0;
    int height = 0;
    int area = 0;
    double cx = 0.0;
    double cy = 0.0;
};

struct LineModel {
    int line_count = 0;
    int component_count = 0;
    double median_component_height = 0.0;
    double layout_score = 0.0;
    double upright_score = 0.0;
    double combined_score = 0.0;
};

struct LineModelSet {
    std::array<LineModel, 4> models;
    int best_index = 0;
    double best_score = 0.0;
    double score_margin = 0.0;
};

cv::Mat rotate_quadrant(const cv::Mat &image, int degrees);

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

double clamp_unit(double value) {
    return std::max(-1.0, std::min(1.0, value));
}

double median_int(std::vector<int> values) {
    if (values.empty()) {
        return 0.0;
    }
    std::sort(values.begin(), values.end());
    size_t mid = values.size() / 2;
    if (values.size() % 2 == 1) {
        return static_cast<double>(values[mid]);
    }
    return (static_cast<double>(values[mid - 1]) + static_cast<double>(values[mid])) / 2.0;
}

double standard_deviation(const std::vector<double> &values) {
    if (values.size() < 2) {
        return 0.0;
    }
    double mean = 0.0;
    for (double value : values) {
        mean += value;
    }
    mean /= static_cast<double>(values.size());

    double variance = 0.0;
    for (double value : values) {
        double diff = value - mean;
        variance += diff * diff;
    }
    variance /= static_cast<double>(values.size());
    return std::sqrt(variance);
}

std::vector<TextComponent> extract_text_components(const cv::Mat &mask, cv::Mat *text_mask) {
    std::vector<TextComponent> components;
    if (text_mask != nullptr) {
        *text_mask = cv::Mat::zeros(mask.size(), CV_8U);
    }

    cv::Mat labels, stats, centroids;
    int count = cv::connectedComponentsWithStats(mask, labels, stats, centroids, 8, CV_32S);
    int total_area = mask.rows * mask.cols;

    for (int label = 1; label < count; ++label) {
        int area = stats.at<int>(label, cv::CC_STAT_AREA);
        int width = stats.at<int>(label, cv::CC_STAT_WIDTH);
        int height = stats.at<int>(label, cv::CC_STAT_HEIGHT);
        if (!is_text_like_component(area, width, height, total_area, mask.cols, mask.rows)) {
            continue;
        }

        TextComponent component;
        component.x = stats.at<int>(label, cv::CC_STAT_LEFT);
        component.y = stats.at<int>(label, cv::CC_STAT_TOP);
        component.width = width;
        component.height = height;
        component.area = area;
        component.cx = centroids.at<double>(label, 0);
        component.cy = centroids.at<double>(label, 1);
        components.push_back(component);

        if (text_mask != nullptr) {
            text_mask->setTo(255, labels == label);
        }
    }

    return components;
}

std::vector<std::pair<int, int>> active_row_runs(const cv::Mat &mask, int max_gap) {
    cv::Mat projected;
    cv::reduce(mask, projected, 1, cv::REDUCE_SUM, CV_64F);

    std::vector<std::pair<int, int>> raw_runs;
    bool in_run = false;
    int start = 0;
    for (int y = 0; y < projected.rows; ++y) {
        double pixels = projected.at<double>(y, 0) / 255.0;
        bool active = pixels > 0.0;
        if (active && !in_run) {
            start = y;
            in_run = true;
        } else if (!active && in_run) {
            raw_runs.emplace_back(start, y - 1);
            in_run = false;
        }
    }
    if (in_run) {
        raw_runs.emplace_back(start, projected.rows - 1);
    }

    if (raw_runs.empty()) {
        return raw_runs;
    }

    std::vector<std::pair<int, int>> merged;
    merged.push_back(raw_runs.front());
    for (size_t i = 1; i < raw_runs.size(); ++i) {
        auto &previous = merged.back();
        if (raw_runs[i].first - previous.second - 1 <= max_gap) {
            previous.second = raw_runs[i].second;
        } else {
            merged.push_back(raw_runs[i]);
        }
    }
    return merged;
}

LineModel score_text_line_model(const cv::Mat &mask) {
    LineModel model;

    cv::Mat text_mask;
    std::vector<TextComponent> components = extract_text_components(mask, &text_mask);
    model.component_count = static_cast<int>(components.size());
    if (model.component_count < kMinLineModelTextComponents) {
        return model;
    }

    std::vector<int> heights;
    heights.reserve(components.size());
    for (const auto &component : components) {
        heights.push_back(component.height);
    }
    double median_height = median_int(heights);
    model.median_component_height = median_height;
    if (median_height < 2.0) {
        return model;
    }

    int kernel_width = std::max(3, static_cast<int>(std::round(median_height * 3.0)));
    kernel_width = std::min(kernel_width, std::max(3, mask.cols / 18));
    int kernel_height = std::max(1, static_cast<int>(std::round(median_height * 0.20)));
    kernel_height = std::min(kernel_height, 5);
    cv::Mat kernel = cv::getStructuringElement(cv::MORPH_RECT, cv::Size(kernel_width, kernel_height));
    cv::Mat dilated;
    cv::dilate(text_mask, dilated, kernel);

    int max_gap = std::max(1, static_cast<int>(std::round(median_height * 0.35)));
    std::vector<std::pair<int, int>> runs = active_row_runs(dilated, max_gap);

    double layout_sum = 0.0;
    double upright_sum = 0.0;
    double upright_weight_sum = 0.0;
    int valid_lines = 0;

    for (const auto &run : runs) {
        int run_height = run.second - run.first + 1;
        if (run_height < std::max(2, static_cast<int>(std::round(median_height * 0.35))) ||
            run_height > std::max(12, static_cast<int>(std::round(median_height * 5.0)))) {
            continue;
        }

        int y0 = std::max(0, run.first - 1);
        int y1 = std::min(mask.rows - 1, run.second + 1);
        cv::Mat band = text_mask(cv::Range(y0, y1 + 1), cv::Range::all());
        std::vector<cv::Point> points;
        cv::findNonZero(band, points);
        if (points.empty()) {
            continue;
        }

        int min_x = mask.cols;
        int max_x = 0;
        int min_y = y1 - y0 + 1;
        int max_y = 0;
        double y_sum = 0.0;
        std::vector<double> row_counts(static_cast<size_t>(y1 - y0 + 1), 0.0);
        for (const auto &point : points) {
            min_x = std::min(min_x, point.x);
            max_x = std::max(max_x, point.x);
            min_y = std::min(min_y, point.y);
            max_y = std::max(max_y, point.y);
            y_sum += static_cast<double>(point.y);
            row_counts[static_cast<size_t>(point.y)] += 1.0;
        }

        int tight_y0 = y0 + min_y;
        int tight_y1 = y0 + max_y;
        int line_height = tight_y1 - tight_y0 + 1;
        int line_width = max_x - min_x + 1;
        if (line_height < 2 || line_width < std::max(24, static_cast<int>(std::round(median_height * 4.0)))) {
            continue;
        }

        int component_count = 0;
        std::vector<double> component_tops;
        std::vector<double> component_bottoms;
        for (const auto &component : components) {
            bool y_overlap = component.y <= tight_y1 + 1 &&
                             (component.y + component.height - 1) >= tight_y0 - 1;
            bool x_overlap = component.x <= max_x + 2 &&
                             (component.x + component.width - 1) >= min_x - 2;
            if (y_overlap && x_overlap) {
                component_count += 1;
                bool edge_candidate =
                    component.height >= std::max(2.0, median_height * 0.45) &&
                    component.height <= std::max(6.0, median_height * 3.5) &&
                    component.width <= std::max(8.0, median_height * 8.0);
                if (edge_candidate) {
                    component_tops.push_back(static_cast<double>(component.y - tight_y0));
                    component_bottoms.push_back(
                        static_cast<double>(component.y + component.height - 1 - tight_y0));
                }
            }
        }
        if (component_count < 3) {
            continue;
        }

        double width_score = std::min(1.0, static_cast<double>(line_width) /
                                               std::max(1.0, mask.cols * 0.28));
        double component_score = std::min(1.0, static_cast<double>(component_count) / 10.0);
        double height_ratio = static_cast<double>(line_height) / std::max(1.0, median_height);
        double height_score = std::exp(-std::abs(height_ratio - 1.35) * 0.30);
        double line_score = width_score * (0.35 + 0.65 * component_score) * height_score;
        if (line_score < 0.08) {
            continue;
        }

        double centroid_norm =
            (y_sum / static_cast<double>(points.size()) - static_cast<double>(min_y)) /
            std::max(1.0, static_cast<double>(line_height - 1));
        double centroid_score = (0.5 - centroid_norm) * 2.0;

        int peak_y = min_y;
        double peak_count = -std::numeric_limits<double>::infinity();
        for (int row = min_y; row <= max_y; ++row) {
            double count = row_counts[static_cast<size_t>(row)];
            if (count > peak_count) {
                peak_count = count;
                peak_y = row;
            }
        }
        double peak_norm = static_cast<double>(peak_y - min_y) /
                           std::max(1.0, static_cast<double>(line_height - 1));
        double peak_score = (peak_norm - 0.5) * 2.0;

        double edge_alignment_score = 0.0;
        if (component_tops.size() >= 5) {
            double top_std = standard_deviation(component_tops);
            double bottom_std = standard_deviation(component_bottoms);
            // In upright Latin-like text, component bottoms tend to align on a
            // baseline more tightly than component tops. Rotating 180 degrees
            // reverses that relation. This is only a weak vote because all-caps,
            // math, handwriting, and non-Latin scripts can be symmetric.
            edge_alignment_score = (top_std - bottom_std) / (top_std + bottom_std + 1.0);
        }

        double line_upright =
            clamp_unit(0.60 * edge_alignment_score + 0.25 * centroid_score + 0.15 * peak_score);

        layout_sum += line_score;
        upright_sum += line_upright * line_score;
        upright_weight_sum += line_score;
        valid_lines += 1;
    }

    model.line_count = valid_lines;
    if (valid_lines == 0 || upright_weight_sum <= 0.0) {
        return model;
    }

    model.layout_score = layout_sum;
    model.upright_score = clamp_unit(upright_sum / upright_weight_sum);
    model.combined_score = model.layout_score * (1.0 + kLineModelUprightWeight * model.upright_score);
    return model;
}

LineModelSet analyze_line_models(const cv::Mat &mask) {
    LineModelSet result;

    for (size_t i = 0; i < kCandidateRotations.size(); ++i) {
        cv::Mat oriented = rotate_quadrant(mask, kCandidateRotations[i]);
        result.models[i] = score_text_line_model(oriented);
    }

    std::array<int, 4> order = {0, 1, 2, 3};
    std::sort(order.begin(), order.end(), [&](int lhs, int rhs) {
        return result.models[static_cast<size_t>(lhs)].combined_score >
               result.models[static_cast<size_t>(rhs)].combined_score;
    });

    result.best_index = order[0];
    result.best_score = result.models[static_cast<size_t>(order[0])].combined_score;
    double second_score = result.models[static_cast<size_t>(order[1])].combined_score;
    result.score_margin = result.best_score - second_score;
    return result;
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

    LineModelSet line_models = analyze_line_models(mask);
    analysis.line_model_best_rotation = kCandidateRotations[static_cast<size_t>(line_models.best_index)];
    analysis.line_model_best_score = line_models.best_score;
    analysis.line_model_score_margin = line_models.score_margin;
    analysis.line_model_lines = line_models.models[static_cast<size_t>(line_models.best_index)].line_count;
    analysis.line_model_components =
        line_models.models[static_cast<size_t>(line_models.best_index)].component_count;
    for (size_t i = 0; i < kCandidateRotations.size(); ++i) {
        analysis.line_model_scores[i] = line_models.models[i].combined_score;
        analysis.line_model_layout_scores[i] = line_models.models[i].layout_score;
        analysis.line_model_upright_scores[i] = line_models.models[i].upright_score;
    }

    double portrait_layout =
        std::max(line_models.models[0].layout_score, line_models.models[2].layout_score);
    double landscape_layout =
        std::max(line_models.models[1].layout_score, line_models.models[3].layout_score);
    bool line_model_has_evidence =
        analysis.line_model_components >= kMinLineModelTextComponents &&
        analysis.line_model_lines >= kMinLineModelLines &&
        analysis.line_model_best_score > 0.0;
    bool line_model_supports_sideways =
        line_model_has_evidence &&
        (analysis.line_model_best_rotation == 90 || analysis.line_model_best_rotation == 270) &&
        analysis.line_model_score_margin >= kLineModelSidewaysMargin &&
        landscape_layout >= portrait_layout * kLineModelSidewaysLayoutRatio;
    bool line_model_supports_180 =
        line_model_has_evidence &&
        analysis.line_model_best_rotation == 180 &&
        analysis.line_model_score_margin >= kLineModelUpsideDownMargin;
    bool line_model_prefers_upright =
        line_model_has_evidence &&
        analysis.line_model_scores[0] >=
            analysis.line_model_scores[2] + kLineModelContradictionMargin;

    bool sideways = analysis.vertical_score > analysis.horizontal_score * kSidewaysProjectionRatio &&
                    (analysis.vertical_score - analysis.horizontal_score) > kSidewaysMinDelta;

    if (line_model_supports_sideways) {
        analysis.rotation = analysis.line_model_best_rotation;
        analysis.confidence = std::min(1.0, analysis.line_model_score_margin);
        analysis.reason = "line_model_sideways";
        return analysis;
    }

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
        analysis.reason = line_model_has_evidence ? "line_model_sideways_confidence_skip"
                                                  : "sideways_low_upright_confidence_skip";
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

        if (line_model_prefers_upright && !line_model_supports_180) {
            analysis.confidence = std::min(1.0, analysis.line_model_score_margin);
            analysis.reason = "line_model_upright_skip";
            return analysis;
        }

        analysis.rotation = 180;
        analysis.confidence = std::min(
            1.0, std::max(-analysis.upright_score, analysis.line_model_score_margin));
        analysis.reason =
            line_model_supports_180 ? "line_model_upside_down" : "bottom_heavy_upside_down";
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
              << "\"line_model_best_rotation\":" << analysis.line_model_best_rotation << ","
              << "\"line_model_lines\":" << analysis.line_model_lines << ","
              << "\"line_model_components\":" << analysis.line_model_components << ","
              << "\"line_model_best_score\":" << analysis.line_model_best_score << ","
              << "\"line_model_score_margin\":" << analysis.line_model_score_margin << ","
              << "\"line_model_score_0\":" << analysis.line_model_scores[0] << ","
              << "\"line_model_score_90\":" << analysis.line_model_scores[1] << ","
              << "\"line_model_score_180\":" << analysis.line_model_scores[2] << ","
              << "\"line_model_score_270\":" << analysis.line_model_scores[3] << ","
              << "\"line_model_layout_0\":" << analysis.line_model_layout_scores[0] << ","
              << "\"line_model_layout_90\":" << analysis.line_model_layout_scores[1] << ","
              << "\"line_model_layout_180\":" << analysis.line_model_layout_scores[2] << ","
              << "\"line_model_layout_270\":" << analysis.line_model_layout_scores[3] << ","
              << "\"line_model_upright_0\":" << analysis.line_model_upright_scores[0] << ","
              << "\"line_model_upright_90\":" << analysis.line_model_upright_scores[1] << ","
              << "\"line_model_upright_180\":" << analysis.line_model_upright_scores[2] << ","
              << "\"line_model_upright_270\":" << analysis.line_model_upright_scores[3] << ","
              << "\"reason\":\"" << json_escape(analysis.reason) << "\""
              << "}\n";
    return 0;
}
