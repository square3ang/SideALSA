#include <QApplication>
#include <QAbstractSpinBox>
#include <QCheckBox>
#include <QCloseEvent>
#include <QComboBox>
#include <QCommandLineOption>
#include <QCommandLineParser>
#include <QFont>
#include <QFormLayout>
#include <QFrame>
#include <QGridLayout>
#include <QHash>
#include <QHBoxLayout>
#include <QIcon>
#include <QLabel>
#include <QLineEdit>
#include <QMainWindow>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QRegularExpression>
#include <QRegularExpressionValidator>
#include <QScrollArea>
#include <QSpinBox>
#include <QStyle>
#include <QStringList>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>
#include <QWheelEvent>
#include <QWidget>

#include <algorithm>
#include <array>
#include <limits>

namespace {

constexpr int kClientRefreshRequiredErrorExitCode = 2;
constexpr std::array<int, 6> kStandardBuffers = {32, 64, 128, 256, 512, 1024};

uint parseDisplayRate(const QString &text)
{
    bool ok = false;
    const uint rate = text.trimmed().toUInt(&ok);
    return (ok && rate > 0) ? rate : 48000U;
}

int periodMicros(int frames, uint rate)
{
    if (frames <= 0 || rate == 0)
        return 0;
    return static_cast<int>((static_cast<quint64>(frames) * 1000000ULL) / rate);
}

int syconAutoHandoffMicros(int frames, uint rate)
{
    if (frames <= 0 || rate == 0)
        return 0;
    const int reserve = std::min((frames + 15) / 16, static_cast<int>((rate + 11999U) / 12000U));
    const int usable = std::max(0, frames - reserve);
    return static_cast<int>((static_cast<quint64>(usable) * 1000000ULL) / rate);
}

int syconSafeHandoffMicros(int frames, uint rate)
{
    if (frames <= 0 || rate == 0)
        return 2000;
    const quint64 scaled = (static_cast<quint64>(frames) * 1500000ULL + rate - 1) / rate;
    return static_cast<int>(std::clamp<quint64>(scaled, 1000ULL, 100000ULL));
}

QString formatBufferChoice(int frames, uint rate)
{
    const double ms = (rate > 0) ? (static_cast<double>(frames) * 1000.0 / rate) : 0.0;
    return QStringLiteral("%1 frames (%2 ms)").arg(frames).arg(ms, 0, 'f', 2);
}

QString optionalNumber(const QSpinBox *box)
{
    return box->value() == 0 ? QStringLiteral("auto") : QString::number(box->value());
}

bool textBool(const QString &value)
{
    return value == QStringLiteral("true");
}

QHash<QString, QString> parseSettings(const QByteArray &output)
{
    QHash<QString, QString> settings;
    for (const QByteArray &line : output.split('\n')) {
        const qsizetype separator = line.indexOf('=');
        if (separator <= 0)
            continue;
        settings.insert(QString::fromUtf8(line.first(separator)),
                        QString::fromUtf8(line.sliced(separator + 1)));
    }
    return settings;
}

QString helperPath()
{
    const QString override = qEnvironmentVariable("SIDEALSA_ADMIN_HELPER");
    if (!override.isEmpty())
        return override;
    return QStringLiteral("/usr/libexec/sidealsa-admin");
}

QWidget *settingsPage(const QString &title, const QString &description, QLayout *content)
{
    auto *page = new QWidget;
    page->setObjectName(QStringLiteral("settingsPage"));
    auto *layout = new QVBoxLayout(page);
    layout->setContentsMargins(22, 20, 22, 22);
    layout->setSpacing(9);

    auto *heading = new QLabel(title);
    heading->setObjectName(QStringLiteral("sectionTitle"));
    layout->addWidget(heading);
    if (!description.isEmpty()) {
        auto *copy = new QLabel(description);
        copy->setObjectName(QStringLiteral("sectionCopy"));
        copy->setWordWrap(true);
        layout->addWidget(copy);
        layout->addSpacing(4);
    }
    layout->addLayout(content);
    layout->addStretch();
    return page;
}

class NumberBox final : public QSpinBox {
protected:
    void wheelEvent(QWheelEvent *event) override
    {
        if (!hasFocus()) {
            event->ignore();
            return;
        }
        QSpinBox::wheelEvent(event);
    }
};

class ChoiceBox final : public QComboBox {
protected:
    void wheelEvent(QWheelEvent *event) override
    {
        if (!hasFocus()) {
            event->ignore();
            return;
        }
        QComboBox::wheelEvent(event);
    }
};

void configureField(QWidget *field)
{
    field->setMinimumWidth(180);
    field->setMaximumWidth(360);
}

QSpinBox *numberBox(int minimum, int maximum, const QString &suffix = {})
{
    auto *box = new NumberBox;
    box->setRange(minimum, maximum);
    box->setAccelerated(true);
    // Qt 6.12 shrinks the editor to 1 px with NoButtons plus a styled frame;
    // the stylesheet hides the buttons instead and the margins replace padding.
    box->findChild<QLineEdit *>()->setTextMargins(10, 0, 10, 0);
    configureField(box);
    if (!suffix.isEmpty())
        box->setSuffix(suffix);
    return box;
}

QSpinBox *optionalNumberBox(int maximum, const QString &suffix = {})
{
    auto *box = numberBox(0, maximum, suffix);
    box->setSpecialValueText(QStringLiteral("Auto"));
    return box;
}

class ControlWindow final : public QMainWindow {
public:
    ControlWindow(QString profilePath, QString socketPath)
        : profilePath_(std::move(profilePath)), socketPath_(std::move(socketPath)),
          helperPath_(helperPath())
    {
        setWindowTitle(QStringLiteral("SideALSA Control"));
        resize(580, 420);
        setMinimumSize(500, 380);
        buildUi();
        loadProfile();
    }

protected:
    void closeEvent(QCloseEvent *event) override
    {
        if (applyProcess_) {
            QMessageBox::information(this, QStringLiteral("Configuration is applying"),
                                     QStringLiteral("Wait for the daemon and client service restarts to finish."));
            event->ignore();
            return;
        }
        if (hasUnsavedChanges()) {
            const auto answer = QMessageBox::question(
                this, QStringLiteral("Discard unsaved changes?"),
                QStringLiteral("Close the control panel and discard the values currently shown?"),
                QMessageBox::Discard | QMessageBox::Cancel, QMessageBox::Cancel);
            if (answer != QMessageBox::Discard) {
                event->ignore();
                return;
            }
        }
        event->accept();
    }

private:
    void buildUi()
    {
        auto *central = new QWidget;
        auto *windowLayout = new QVBoxLayout(central);
        windowLayout->setContentsMargins(0, 0, 0, 0);
        windowLayout->setSpacing(0);

        auto *scroll = new QScrollArea;
        scroll->setWidgetResizable(true);
        scroll->setFrameShape(QFrame::NoFrame);
        scroll->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
        auto *page = new QWidget;
        auto *root = new QVBoxLayout(page);
        root->setContentsMargins(24, 22, 24, 20);
        root->setSpacing(14);

        auto *hero = new QFrame;
        hero->setObjectName(QStringLiteral("hero"));
        auto *heroLayout = new QGridLayout(hero);
        heroLayout->setContentsMargins(22, 18, 22, 18);
        heroLayout->setColumnStretch(0, 1);

        auto *eyebrow = new QLabel(QStringLiteral("SIDEALSA AUDIO"));
        eyebrow->setObjectName(QStringLiteral("eyebrow"));
        auto *title = new QLabel(QStringLiteral("SideALSA Control"));
        title->setObjectName(QStringLiteral("title"));
        subtitleLabel_ = new QLabel(
            QStringLiteral("Choose buffer size and stability mode. Changes apply cleanly without restarting PipeWire."));
        subtitleLabel_->setObjectName(QStringLiteral("subtitle"));
        subtitleLabel_->setWordWrap(true);
        statusBadge_ = new QLabel(QStringLiteral("Checking daemon"));
        statusBadge_->setObjectName(QStringLiteral("statusBadge"));
        statusBadge_->setProperty("state", "idle");
        statusBadge_->setAlignment(Qt::AlignCenter);
        statusBadge_->setMinimumWidth(132);

        heroLayout->addWidget(eyebrow, 0, 0);
        heroLayout->addWidget(statusBadge_, 0, 1, 2, 1, Qt::AlignRight | Qt::AlignTop);
        heroLayout->addWidget(title, 1, 0);
        heroLayout->addWidget(subtitleLabel_, 2, 0, 1, 2);
        root->addWidget(hero);

        rate_ = new ChoiceBox;
        rate_->setObjectName(QStringLiteral("rate"));
        configureField(rate_);
        rate_->setEditable(true);
        rate_->setInsertPolicy(QComboBox::NoInsert);
        rate_->addItems({QStringLiteral("44100"), QStringLiteral("48000"),
                         QStringLiteral("88200"), QStringLiteral("96000"),
                         QStringLiteral("176400"), QStringLiteral("192000")});
        rate_->lineEdit()->setValidator(
            new QRegularExpressionValidator(QRegularExpression(QStringLiteral("[1-9][0-9]{0,9}")), rate_));
        rate_->setToolTip(QStringLiteral("Physical playback and capture sample rate (Hz)"));

        simpleBufferSize_ = new ChoiceBox;
        simpleBufferSize_->setObjectName(QStringLiteral("simpleBufferSize"));
        configureField(simpleBufferSize_);
        populateSimpleBuffers(48000U, 64);

        safeMode_ = new QCheckBox(QStringLiteral("Safe mode (extra headroom for DSP spikes)"));
        safeMode_->setToolTip(
            QStringLiteral("Adds a small output buffer margin to absorb occasional DAW/Wine spikes. "
                           "Disable for minimum latency."));
        modeHint_ = new QLabel;
        modeHint_->setObjectName(QStringLiteral("sectionCopy"));
        modeHint_->setWordWrap(true);

        simpleCard_ = new QFrame;
        simpleCard_->setObjectName(QStringLiteral("simpleCard"));
        auto *simpleForm = new QFormLayout(simpleCard_);
        simpleForm->setContentsMargins(20, 18, 20, 18);
        simpleForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
        simpleForm->setHorizontalSpacing(22);
        simpleForm->setVerticalSpacing(12);
        simpleForm->addRow(QStringLiteral("Sample rate"), rate_);
        simpleForm->addRow(QStringLiteral("Buffer size"), simpleBufferSize_);
        simpleForm->addRow(QString(), safeMode_);
        simpleForm->addRow(QString(), modeHint_);
        root->addWidget(simpleCard_);

        advancedToggle_ = new QCheckBox(QStringLiteral("Show advanced settings"));
        advancedToggle_->setObjectName(QStringLiteral("advancedToggle"));
        root->addWidget(advancedToggle_);

        settingsTabs_ = new QTabWidget;
        settingsTabs_->setObjectName(QStringLiteral("settingsTabs"));
        settingsTabs_->setDocumentMode(true);
        settingsTabs_->setVisible(false);

        constexpr int maximumInteger = std::numeric_limits<int>::max();
        periodSize_ = numberBox(1, maximumInteger, QStringLiteral(" frames"));
        periodSize_->setObjectName(QStringLiteral("periodSize"));
        hardwarePeriodSize_ = optionalNumberBox(maximumInteger, QStringLiteral(" frames"));
        bufferSize_ = numberBox(1, maximumInteger, QStringLiteral(" frames"));
        sharedBufferSize_ = optionalNumberBox(maximumInteger, QStringLiteral(" frames"));
        playbackQueuePeriods_ = optionalNumberBox(maximumInteger, QStringLiteral(" periods"));

        auto *clockForm = new QFormLayout;
        clockForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
        clockForm->setHorizontalSpacing(24);
        clockForm->setVerticalSpacing(11);
        clockForm->addRow(QStringLiteral("Client period"), periodSize_);
        clockForm->addRow(QStringLiteral("Physical period"), hardwarePeriodSize_);
        clockForm->addRow(QStringLiteral("Hardware buffer"), bufferSize_);
        clockForm->addRow(QStringLiteral("Shared buffer"), sharedBufferSize_);
        clockForm->addRow(QStringLiteral("Playback queue"), playbackQueuePeriods_);
        settingsTabs_->addTab(
            settingsPage(
                QStringLiteral("Buffers"),
                QStringLiteral("Raw period and ring geometry. Auto derives values from Buffer size."),
                clockForm),
            QStringLiteral("&Buffers"));

        playbackTimerScheduling_ = new QCheckBox(QStringLiteral("Use timer-driven playback scheduling"));
        duplexLink_ = new ChoiceBox;
        configureField(duplexLink_);
        duplexLink_->addItem(QStringLiteral("Auto"), QStringLiteral("auto"));
        duplexLink_->addItem(QStringLiteral("Linked"), QStringLiteral("true"));
        duplexLink_->addItem(QStringLiteral("Independent"), QStringLiteral("false"));
        linkedGuardFrames_ = optionalNumberBox(maximumInteger, QStringLiteral(" frames"));
        linkedPhaseAttempts_ = numberBox(0, 64, QStringLiteral(" attempts"));

        auto *duplexForm = new QFormLayout;
        duplexForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
        duplexForm->setHorizontalSpacing(24);
        duplexForm->setVerticalSpacing(11);
        duplexForm->addRow(QString(), playbackTimerScheduling_);
        duplexForm->addRow(QStringLiteral("Duplex mode"), duplexLink_);
        duplexForm->addRow(QStringLiteral("Playback guard"), linkedGuardFrames_);
        duplexForm->addRow(QStringLiteral("Phase search"), linkedPhaseAttempts_);
        settingsTabs_->addTab(
            settingsPage(
                QStringLiteral("Duplex timeline"),
                QStringLiteral("Controls linked capture/playback scheduling and zero-lead write safety."),
                duplexForm),
            QStringLiteral("&Duplex"));

        proLatencyPeriods_ = numberBox(0, 7, QStringLiteral(" periods"));
        proHandoffUs_ = numberBox(0, maximumInteger, QStringLiteral(" us"));
        proHandoffUs_->setToolTip(QStringLiteral("Fixed handoff ceiling. Ignored in direct zero-lead mode when Automatic PRO handoff is enabled."));
        proHandoffAuto_ = new QCheckBox(QStringLiteral("Automatic PRO handoff (scales with buffer period)"));
        proHandoffAuto_->setToolTip(QStringLiteral("Direct zero-lead mode: one logical period minus write reserve. ALSA Q64/48k uses 1.00 ms; Sycon Q64/48k uses 1.25 ms. Disable for manual headroom (Safe mode)."));
        proRealtimePriority_ = optionalNumberBox(99);
        sharedLatencyPeriods_ = numberBox(0, 7, QStringLiteral(" periods"));
        sharedPlaybackRepeatOnUnderrun_ =
            new QCheckBox(QStringLiteral("Repeat the last SHARED period until playback resumes"));
        sharedPlaybackRepeatOnUnderrun_->setToolTip(
            QStringLiteral("Can hide short gaps, but a long outage may produce a repeating tone."));
        realtime_ = new QCheckBox(QStringLiteral("Enable realtime hardware thread"));
        realtimePriority_ = numberBox(1, 99);

        auto *schedulingForm = new QFormLayout;
        schedulingForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
        schedulingForm->setHorizontalSpacing(24);
        schedulingForm->setVerticalSpacing(11);
        schedulingForm->addRow(QStringLiteral("PRO lead"), proLatencyPeriods_);
        schedulingForm->addRow(QStringLiteral("Manual PRO handoff"), proHandoffUs_);
        schedulingForm->addRow(QString(), proHandoffAuto_);
        schedulingForm->addRow(QStringLiteral("PRO RT priority"), proRealtimePriority_);
        schedulingForm->addRow(QStringLiteral("SHARED lead"), sharedLatencyPeriods_);
        schedulingForm->addRow(QString(), sharedPlaybackRepeatOnUnderrun_);
        schedulingForm->addRow(QString(), realtime_);
        schedulingForm->addRow(QStringLiteral("Hardware RT priority"), realtimePriority_);
        settingsTabs_->addTab(
            settingsPage(
                QStringLiteral("Scheduling"),
                QStringLiteral("Deadline budget and thread priorities. Invalid combinations are rejected before restart."),
                schedulingForm),
            QStringLiteral("&Scheduling"));
        root->addWidget(settingsTabs_, 1);

        scroll->setWidget(page);
        windowLayout->addWidget(scroll, 1);

        auto *footer = new QFrame;
        footer->setObjectName(QStringLiteral("footer"));
        auto *footerLayout = new QGridLayout(footer);
        footerLayout->setContentsMargins(24, 12, 24, 12);
        footerLayout->setHorizontalSpacing(16);
        footerLayout->setColumnStretch(0, 1);
        detailLabel_ = new QLabel;
        detailLabel_->setObjectName(QStringLiteral("detail"));
        detailLabel_->setWordWrap(true);
        detailLabel_->setMinimumHeight(36);
        reloadButton_ = new QPushButton(QStringLiteral("&Reload saved"));
        reloadButton_->setObjectName(QStringLiteral("reloadButton"));
        reloadButton_->setIcon(style()->standardIcon(QStyle::SP_BrowserReload));
        reloadButton_->setToolTip(QStringLiteral("Reload saved values and refresh daemon status"));
        applyButton_ = new QPushButton(QStringLiteral("&Apply configuration"));
        applyButton_->setObjectName(QStringLiteral("primaryButton"));
        applyButton_->setIcon(style()->standardIcon(QStyle::SP_DialogApplyButton));
        applyButton_->setDefault(true);
        auto *footerActions = new QHBoxLayout;
        footerActions->setContentsMargins(0, 0, 0, 0);
        footerActions->setSpacing(8);
        footerActions->addWidget(reloadButton_);
        footerActions->addWidget(applyButton_);
        footerLayout->addWidget(detailLabel_, 0, 0);
        footerLayout->addLayout(footerActions, 0, 1, Qt::AlignRight | Qt::AlignVCenter);
        windowLayout->addWidget(footer);

        connect(reloadButton_, &QPushButton::clicked, this, [this] { reloadProfile(); });
        connect(applyButton_, &QPushButton::clicked, this, [this] { applyProfile(); });
        connect(advancedToggle_, &QCheckBox::toggled, this, [this](bool show) {
            settingsTabs_->setVisible(show);
            if (show) {
                resize(std::max(width(), 780), std::max(height(), 680));
            } else {
                resize(580, 420);
            }
        });
        connect(simpleBufferSize_, qOverload<int>(&QComboBox::currentIndexChanged), this,
                [this](int index) {
                    if (loadingWidgets_ || syncingSimple_ || index < 0)
                        return;
                    const int period = simpleBufferSize_->itemData(index).toInt();
                    if (period > 0)
                        applySimpleBufferSize(period);
                });
        connect(safeMode_, &QCheckBox::toggled, this, [this](bool checked) {
            if (loadingWidgets_ || syncingSimple_)
                return;
            applySimpleSafeMode(checked);
        });

        const auto watchSpinBox = [this](QSpinBox *box) {
            connect(box, qOverload<int>(&QSpinBox::valueChanged), this, [this] {
                if (loadingWidgets_ || syncingSimple_)
                    return;
                syncSimpleFromAdvanced();
                markEdited();
            });
        };
        for (QSpinBox *box : {periodSize_, hardwarePeriodSize_, bufferSize_, sharedBufferSize_,
                              playbackQueuePeriods_, linkedGuardFrames_, linkedPhaseAttempts_,
                              proLatencyPeriods_, proHandoffUs_, proRealtimePriority_,
                              sharedLatencyPeriods_, realtimePriority_}) {
            watchSpinBox(box);
        }
        connect(rate_, &QComboBox::currentTextChanged, this, [this] {
            if (loadingWidgets_ || syncingSimple_)
                return;
            onRateChanged();
        });
        connect(duplexLink_, qOverload<int>(&QComboBox::currentIndexChanged), this,
                [this] { markEdited(); });
        connect(playbackTimerScheduling_, &QCheckBox::toggled, this,
                [this] { markEdited(); });
        connect(sharedPlaybackRepeatOnUnderrun_, &QCheckBox::toggled, this,
                [this] { markEdited(); });
        connect(realtime_, &QCheckBox::toggled, this, [this] { markEdited(); });
        connect(proHandoffAuto_, &QCheckBox::toggled, this, [this] {
            if (loadingWidgets_ || syncingSimple_)
                return;
            syncSimpleFromAdvanced();
            markEdited();
        });

        setCentralWidget(central);
        setStyleSheet(QStringLiteral(R"(
            QMainWindow { background: palette(window); }
            QFrame#hero, QFrame#simpleCard {
                background: palette(base);
                border: 1px solid palette(mid);
                border-radius: 12px;
            }
            QFrame#footer {
                background: palette(base);
                border-top: 1px solid palette(mid);
            }
            QTabWidget#settingsTabs::pane {
                background: palette(base);
                border: 1px solid palette(mid);
                border-radius: 10px;
                top: -1px;
            }
            QTabWidget#settingsTabs QTabBar::tab {
                min-width: 92px;
                padding: 9px 16px;
                margin-right: 4px;
                color: palette(placeholder-text);
                background: transparent;
                border: 1px solid transparent;
                border-bottom: none;
                border-top-left-radius: 7px;
                border-top-right-radius: 7px;
            }
            QTabWidget#settingsTabs QTabBar::tab:selected {
                color: palette(text);
                background: palette(base);
                border-color: palette(mid);
                font-weight: 650;
            }
            QTabWidget#settingsTabs QTabBar::tab:hover:!selected {
                color: palette(text);
                background: palette(alternate-base);
            }
            QLabel#eyebrow {
                color: palette(highlight);
                font-size: 10px;
                font-weight: 700;
                letter-spacing: 1px;
            }
            QLabel#title { font-size: 25px; font-weight: 700; }
            QLabel#subtitle, QLabel#sectionCopy, QLabel#detail { color: palette(placeholder-text); }
            QLabel#sectionTitle { font-size: 16px; font-weight: 650; }
            QLabel#statusBadge {
                border-radius: 9px;
                padding: 6px 11px;
                font-weight: 650;
            }
            QLabel#statusBadge[state="ok"] {
                color: palette(highlighted-text);
                background: palette(highlight);
            }
            QLabel#statusBadge[state="warning"] {
                color: palette(text);
                background: palette(alternate-base);
                border: 1px solid palette(mid);
            }
            QLabel#statusBadge[state="error"] {
                color: #ffffff;
                background: #b3261e;
            }
            QComboBox, QSpinBox {
                min-height: 32px;
                padding: 0 10px;
                background: palette(base);
                border: 1px solid palette(mid);
                border-radius: 6px;
            }
            QSpinBox::up-button, QSpinBox::down-button { width: 0; border: none; }
            QComboBox:focus, QSpinBox:focus {
                border-color: palette(highlight);
            }
            QCheckBox { spacing: 8px; }
            QPushButton {
                min-height: 34px;
                padding: 0 15px;
                background: palette(button);
                border: 1px solid palette(mid);
                border-radius: 6px;
            }
            QPushButton:hover { border-color: palette(highlight); }
            QPushButton#primaryButton {
                color: palette(highlighted-text);
                background: palette(highlight);
                border: 1px solid palette(highlight);
                border-radius: 6px;
                font-weight: 650;
            }
            QPushButton#primaryButton:disabled { background: palette(mid); border-color: palette(mid); }
        )"));
    }

    void setStatus(const QString &text, const char *state)
    {
        statusBadge_->setText(text);
        statusBadge_->setProperty("state", state);
        statusBadge_->style()->unpolish(statusBadge_);
        statusBadge_->style()->polish(statusBadge_);
    }

    void setBusy(bool busy)
    {
        busy_ = busy;
        simpleCard_->setEnabled(!busy);
        advancedToggle_->setEnabled(!busy);
        settingsTabs_->setEnabled(!busy);
        updateApplyState();
        if (busy)
            setStatus(QStringLiteral("Applying"), "warning");
    }

    void populateSimpleBuffers(uint rate, int currentPeriod)
    {
        simpleBufferSize_->clear();
        bool found = false;
        for (int frames : kStandardBuffers) {
            simpleBufferSize_->addItem(formatBufferChoice(frames, rate), frames);
            if (frames == currentPeriod)
                found = true;
        }
        if (currentPeriod > 0 && !found) {
            simpleBufferSize_->addItem(
                formatBufferChoice(currentPeriod, rate) + QStringLiteral(" (custom)"),
                currentPeriod);
        }
        const int idx = simpleBufferSize_->findData(currentPeriod);
        if (idx >= 0)
            simpleBufferSize_->setCurrentIndex(idx);
    }

    void onRateChanged()
    {
        syncingSimple_ = true;
        const uint rate = parseDisplayRate(rate_->currentText());
        const int period = periodSize_->value();
        populateSimpleBuffers(rate, period);
        if (safeMode_->isChecked()) {
            if (isSycon_) {
                proHandoffUs_->setValue(syconSafeHandoffMicros(period, rate));
            } else {
                proHandoffUs_->setValue(periodMicros(period, rate));
            }
        }
        updateModeHint();
        syncingSimple_ = false;
        markEdited();
    }

    void applySimpleBufferSize(int period)
    {
        syncingSimple_ = true;
        periodSize_->setValue(period);
        // Derive a valid ALSA/Sycon ring geometry automatically.
        hardwarePeriodSize_->setValue(0);
        bufferSize_->setValue(period * 4);
        sharedBufferSize_->setValue(0);
        syncingSimple_ = false;
        applySimpleSafeMode(safeMode_->isChecked());
    }

    void applySimpleSafeMode(bool safe)
    {
        syncingSimple_ = true;
        const uint rate = parseDisplayRate(rate_->currentText());
        const int period = periodSize_->value();
        if (isSycon_) {
            proLatencyPeriods_->setValue(0);
            if (safe) {
                proHandoffAuto_->setChecked(false);
                proHandoffUs_->setValue(syconSafeHandoffMicros(period, rate));
            } else {
                proHandoffAuto_->setChecked(true);
            }
        } else {
            linkedGuardFrames_->setValue(0);
            if (safe) {
                proLatencyPeriods_->setValue(1);
                proHandoffAuto_->setChecked(false);
                proHandoffUs_->setValue(periodMicros(period, rate));
                linkedPhaseAttempts_->setValue(0);
            } else {
                proLatencyPeriods_->setValue(0);
                proHandoffAuto_->setChecked(true);
            }
        }
        updateModeHint();
        syncingSimple_ = false;
        markEdited();
    }

    void syncSimpleFromAdvanced()
    {
        const bool prev = syncingSimple_;
        syncingSimple_ = true;
        const uint rate = parseDisplayRate(rate_->currentText());
        const int period = periodSize_->value();
        populateSimpleBuffers(rate, period);
        const bool safe = isSycon_
            ? (!proHandoffAuto_->isChecked()
               && proHandoffUs_->value() > syconAutoHandoffMicros(period, rate))
            : (proLatencyPeriods_->value() > 0);
        safeMode_->setChecked(safe);
        updateModeHint();
        syncingSimple_ = prev;
    }

    void updateModeHint()
    {
        const uint rate = parseDisplayRate(rate_->currentText());
        const int period = periodSize_->value();
        const QString backendText = isSycon_ ? QStringLiteral("Sycon UAC2") : QStringLiteral("ALSA");
        if (isSycon_) {
            if (proHandoffAuto_->isChecked()) {
                const double ms = syconAutoHandoffMicros(period, rate) / 1000.0;
                modeHint_->setText(
                    QStringLiteral("%1 · Low latency · %2 ms DSP budget")
                        .arg(backendText)
                        .arg(ms, 0, 'f', 2));
            } else {
                const double handoffMs = proHandoffUs_->value() / 1000.0;
                const double extraMs =
                    std::max(0, proHandoffUs_->value() - syconAutoHandoffMicros(period, rate))
                    / 1000.0;
                const bool standardSafe = proHandoffUs_->value() == syconSafeHandoffMicros(period, rate);
                modeHint_->setText(
                    QStringLiteral("%1 · %2 · %3 ms headroom (+%4 ms)")
                        .arg(backendText,
                             standardSafe ? QStringLiteral("Safe mode")
                                          : QStringLiteral("Custom headroom"))
                        .arg(handoffMs, 0, 'f', 2)
                        .arg(extraMs, 0, 'f', 2));
            }
        } else {
            if (proLatencyPeriods_->value() == 0) {
                modeHint_->setText(
                    QStringLiteral("%1 · Low latency · direct zero-lead PRO").arg(backendText));
            } else {
                modeHint_->setText(
                    QStringLiteral("%1 · Safe mode · +%2 period PRO buffer")
                        .arg(backendText)
                        .arg(proLatencyPeriods_->value()));
            }
        }
    }

    bool hasUnsavedChanges() const
    {
        return editsPending_;
    }

    void markEdited()
    {
        if (loadingWidgets_)
            return;
        editsPending_ = timingAssignments() != loadedAssignments_;
        updateApplyState();
    }

    void updateApplyState()
    {
        if (loadingWidgets_)
            return;
        const bool dirty = hasUnsavedChanges();
        applyButton_->setText(restartRequired_ && !dirty ? QStringLiteral("&Restart daemon")
                                                         : QStringLiteral("&Apply configuration"));
        applyButton_->setEnabled(!busy_ && profileLoaded_ && (dirty || restartRequired_));
        reloadButton_->setEnabled(!busy_);
    }

    void reloadProfile()
    {
        if (hasUnsavedChanges()) {
            const auto answer = QMessageBox::question(
                this, QStringLiteral("Discard unsaved changes?"),
                QStringLiteral("Reload the saved profile and discard the values currently shown?"),
                QMessageBox::Discard | QMessageBox::Cancel, QMessageBox::Cancel);
            if (answer != QMessageBox::Discard)
                return;
        }
        loadProfile();
    }

    bool refreshProfilePreservingEdits()
    {
        const bool hadPendingEdits = hasUnsavedChanges();
        const bool hadLoadedProfile = profileLoaded_;
        const bool previousRestartRequired = restartRequired_;
        const QString previousRevision = revision_;
        const QStringList previousLoadedAssignments = loadedAssignments_;
        const QHash<QString, QString> pendingValues =
            parseSettings(timingAssignments().join(QLatin1Char('\n')).toUtf8());
        const bool daemonReady = loadProfile();
        loadingWidgets_ = true;
        const bool restored = loadWidgets(pendingValues);
        loadingWidgets_ = false;
        if (!restored) {
            profileLoaded_ = false;
            setStatus(QStringLiteral("Invalid pending values"), "error");
        }
        if (!profileLoaded_ && hadLoadedProfile) {
            profileLoaded_ = true;
            restartRequired_ = previousRestartRequired;
            revision_ = previousRevision;
            loadedAssignments_ = previousLoadedAssignments;
        }
        editsPending_ = profileLoaded_ ? timingAssignments() != loadedAssignments_
                                       : hadPendingEdits;
        updateApplyState();
        return daemonReady;
    }

    bool loadProfile()
    {
        editsPending_ = false;
        profileLoaded_ = false;
        restartRequired_ = false;
        updateApplyState();
        QProcess process;
        QStringList arguments = {QStringLiteral("show")};
        if (!profilePath_.isEmpty())
            arguments << QStringLiteral("--profile") << profilePath_;
        if (!socketPath_.isEmpty())
            arguments << QStringLiteral("--socket") << socketPath_;
        process.start(helperPath_, arguments);
        if (!process.waitForStarted(2000) || !process.waitForFinished(4000)
            || process.exitStatus() != QProcess::NormalExit || process.exitCode() != 0) {
            const QString error = QString::fromUtf8(process.readAllStandardError()).trimmed();
            setStatus(QStringLiteral("Profile unavailable"), "error");
            detailLabel_->setText(error.isEmpty()
                                      ? QStringLiteral("Could not run %1").arg(helperPath_)
                                      : error);
            updateApplyState();
            return false;
        }

        const QHash<QString, QString> values = parseSettings(process.readAllStandardOutput());
        revision_ = values.value(QStringLiteral("revision"));
        const QString resolvedProfile = values.value(QStringLiteral("profile_path"));
        const QString resolvedSocket = values.value(QStringLiteral("socket_path"));
        if (revision_.isEmpty() || resolvedProfile.isEmpty() || resolvedSocket.isEmpty()) {
            setStatus(QStringLiteral("Invalid helper output"), "error");
            detailLabel_->setText(QStringLiteral("The helper did not return a revision and resolved profile/socket paths. Install the matching sidealsa-admin helper."));
            updateApplyState();
            return false;
        }
        profilePath_ = resolvedProfile;
        socketPath_ = resolvedSocket;
        detailLabel_->setToolTip(QStringLiteral("Profile: %1\nSocket: %2").arg(profilePath_, socketPath_));
        loadingWidgets_ = true;
        const bool widgetsLoaded = loadWidgets(values);
        loadingWidgets_ = false;
        if (!widgetsLoaded) {
            setStatus(QStringLiteral("Unsupported profile"), "error");
            detailLabel_->setText(
                QStringLiteral("A timing value is missing or exceeds this control panel's integer range.\n%1")
                    .arg(profilePath_));
            updateApplyState();
            return false;
        }
        profileLoaded_ = true;
        loadedAssignments_ = timingAssignments();
        editsPending_ = false;

        const QString daemonStatus = values.value(QStringLiteral("daemon_status"));
        const bool matches = textBool(values.value(QStringLiteral("daemon_profile_matches")));
        if (daemonStatus == QStringLiteral("active") && matches) {
            setStatus(QStringLiteral("Daemon online"), "ok");
            const QString backendName =
                isSycon_ ? QStringLiteral("Sycon UAC2") : QStringLiteral("ALSA");
            const QString modeName =
                safeMode_->isChecked() ? QStringLiteral("Safe mode") : QStringLiteral("Low latency");
            detailLabel_->setText(
                QStringLiteral("%1  |  PID %2  |  %3 Hz  |  %4 frames  |  %5")
                    .arg(backendName,
                         values.value(QStringLiteral("daemon_pid")),
                         values.value(QStringLiteral("daemon_rate")),
                         values.value(QStringLiteral("daemon_period_size")),
                         modeName));
        } else if (daemonStatus == QStringLiteral("active")) {
            restartRequired_ = true;
            setStatus(QStringLiteral("Restart required"), "warning");
            detailLabel_->setText(QStringLiteral("The saved profile differs from the running daemon.\n%1")
                                      .arg(profilePath_));
            updateApplyState();
            return false;
        } else {
            restartRequired_ = true;
            setStatus(QStringLiteral("Daemon offline"), "error");
            detailLabel_->setText(QStringLiteral("%1\n%2")
                                      .arg(values.value(QStringLiteral("daemon_error")), profilePath_));
            updateApplyState();
            return false;
        }
        updateApplyState();
        return true;
    }

    bool loadWidgets(const QHash<QString, QString> &values)
    {
        if (values.contains(QStringLiteral("backend")))
            isSycon_ = values.value(QStringLiteral("backend")) == QStringLiteral("sycon");
        bool rateOk = false;
        const uint rate = values.value(QStringLiteral("rate")).toUInt(&rateOk);
        if (!rateOk || rate == 0)
            return false;
        rate_->setCurrentText(QString::number(rate));

        bool valid = true;
        valid &= setNumber(periodSize_, values.value(QStringLiteral("period_size")));
        valid &= setOptional(hardwarePeriodSize_,
                             values.value(QStringLiteral("hardware_period_size")));
        valid &= setNumber(bufferSize_, values.value(QStringLiteral("buffer_size")));
        valid &= setOptional(sharedBufferSize_, values.value(QStringLiteral("shared_buffer_size")));
        valid &= setOptional(playbackQueuePeriods_,
                             values.value(QStringLiteral("playback_queue_periods")));
        playbackTimerScheduling_->setChecked(
            textBool(values.value(QStringLiteral("playback_timer_scheduling"))));
        const int duplexIndex = duplexLink_->findData(values.value(QStringLiteral("duplex_link")));
        valid &= duplexIndex >= 0;
        if (duplexIndex >= 0)
            duplexLink_->setCurrentIndex(duplexIndex);
        valid &= setOptional(linkedGuardFrames_,
                             values.value(QStringLiteral("linked_playback_guard_frames")));
        valid &= setNumber(linkedPhaseAttempts_,
                           values.value(QStringLiteral("linked_phase_max_attempts")));
        valid &= setNumber(proLatencyPeriods_, values.value(QStringLiteral("pro_latency_periods")));
        valid &= setNumber(proHandoffUs_, values.value(QStringLiteral("pro_handoff_us")));
        proHandoffAuto_->setChecked(textBool(values.value(QStringLiteral("pro_handoff_auto"))));
        valid &= setOptional(proRealtimePriority_,
                             values.value(QStringLiteral("pro_realtime_priority")));
        valid &= setNumber(sharedLatencyPeriods_,
                           values.value(QStringLiteral("shared_latency_periods")));
        sharedPlaybackRepeatOnUnderrun_->setChecked(
            textBool(values.value(QStringLiteral("shared_playback_repeat_on_underrun"))));
        realtime_->setChecked(textBool(values.value(QStringLiteral("realtime"))));
        valid &= setNumber(realtimePriority_, values.value(QStringLiteral("realtime_priority")));
        if (valid)
            syncSimpleFromAdvanced();
        return valid;
    }

    static bool setNumber(QSpinBox *box, const QString &value)
    {
        bool ok = false;
        const uint parsed = value.toUInt(&ok);
        if (!ok || parsed < static_cast<uint>(box->minimum())
            || parsed > static_cast<uint>(box->maximum()))
            return false;
        box->setValue(static_cast<int>(parsed));
        return true;
    }

    static bool setOptional(QSpinBox *box, const QString &value)
    {
        if (value == QStringLiteral("auto")) {
            box->setValue(0);
            return true;
        }
        return setNumber(box, value);
    }

    QStringList timingAssignments() const
    {
        return {
            QStringLiteral("rate=%1").arg(rate_->currentText().trimmed()),
            QStringLiteral("period_size=%1").arg(periodSize_->value()),
            QStringLiteral("hardware_period_size=%1").arg(optionalNumber(hardwarePeriodSize_)),
            QStringLiteral("buffer_size=%1").arg(bufferSize_->value()),
            QStringLiteral("shared_buffer_size=%1").arg(optionalNumber(sharedBufferSize_)),
            QStringLiteral("playback_queue_periods=%1").arg(optionalNumber(playbackQueuePeriods_)),
            QStringLiteral("playback_timer_scheduling=%1")
                .arg(playbackTimerScheduling_->isChecked() ? QStringLiteral("true")
                                                           : QStringLiteral("false")),
            QStringLiteral("duplex_link=%1").arg(duplexLink_->currentData().toString()),
            QStringLiteral("linked_playback_guard_frames=%1").arg(optionalNumber(linkedGuardFrames_)),
            QStringLiteral("linked_phase_max_attempts=%1").arg(linkedPhaseAttempts_->value()),
            QStringLiteral("pro_latency_periods=%1").arg(proLatencyPeriods_->value()),
            QStringLiteral("pro_handoff_us=%1").arg(proHandoffUs_->value()),
            QStringLiteral("pro_handoff_auto=%1").arg(proHandoffAuto_->isChecked() ? QStringLiteral("true") : QStringLiteral("false")),
            QStringLiteral("pro_realtime_priority=%1").arg(optionalNumber(proRealtimePriority_)),
            QStringLiteral("shared_latency_periods=%1").arg(sharedLatencyPeriods_->value()),
            QStringLiteral("shared_playback_repeat_on_underrun=%1")
                .arg(sharedPlaybackRepeatOnUnderrun_->isChecked() ? QStringLiteral("true")
                                                                  : QStringLiteral("false")),
            QStringLiteral("realtime=%1")
                .arg(realtime_->isChecked() ? QStringLiteral("true") : QStringLiteral("false")),
            QStringLiteral("realtime_priority=%1").arg(realtimePriority_->value()),
        };
    }

    void applyProfile()
    {
        bool rateOk = false;
        const uint rate = rate_->currentText().trimmed().toUInt(&rateOk);
        if (!rateOk || rate == 0) {
            QMessageBox::warning(this, QStringLiteral("Invalid sample rate"),
                                 QStringLiteral("Enter a positive sample rate in hertz."));
            return;
        }
        if (revision_.isEmpty()) {
            loadProfile();
            return;
        }

        const auto answer = QMessageBox::warning(
            this, QStringLiteral("Apply hardware timing?"),
            QStringLiteral("SideALSA will restart the physical stream. PipeWire services will remain running. "
                           "PRO and SHARED connections will be interrupted; affected clients may need to reconnect.\n\n"
                           "If the hardware rejects these values, the current profile is restored automatically."),
            QMessageBox::Apply | QMessageBox::Cancel, QMessageBox::Cancel);
        if (answer != QMessageBox::Apply)
            return;

        QStringList arguments = {
            helperPath_, QStringLiteral("apply"), QStringLiteral("--profile"), profilePath_,
            QStringLiteral("--socket"), socketPath_, QStringLiteral("--expected-revision"), revision_,
        };
        arguments.append(timingAssignments());

        setBusy(true);
        detailLabel_->setText(QStringLiteral("Validating the profile and restarting sidealsad..."));
        applyProcess_ = new QProcess(this);
        connect(applyProcess_, &QProcess::errorOccurred, this,
                [this](QProcess::ProcessError error) {
                    if (error == QProcess::FailedToStart)
                        finishApply(false, false, QStringLiteral("Could not start pkexec."));
                });
        connect(applyProcess_, qOverload<int, QProcess::ExitStatus>(&QProcess::finished), this,
                [this](int exitCode, QProcess::ExitStatus exitStatus) {
                    if (!applyProcess_)
                        return;
                    const QString standardOutput =
                        QString::fromUtf8(applyProcess_->readAllStandardOutput()).trimmed();
                    const QString standardError =
                        QString::fromUtf8(applyProcess_->readAllStandardError()).trimmed();
                    const bool success = exitStatus == QProcess::NormalExit && exitCode == 0;
                    const bool refreshClients = success
                        || (exitStatus == QProcess::NormalExit
                            && exitCode == kClientRefreshRequiredErrorExitCode);
                    QString message = success ? standardOutput : standardError;
                    if (!success && exitStatus == QProcess::CrashExit) {
                        const QString recovery = QStringLiteral(
                            "The apply helper terminated unexpectedly. Check the daemon status and "
                            "reconnect affected clients if needed. PipeWire services were left running.");
                        message = message.isEmpty() ? recovery
                                                    : QStringLiteral("%1\n%2").arg(message, recovery);
                    }
                    finishApply(success, refreshClients, message);
                });
        applyProcess_->start(QStringLiteral("pkexec"), arguments);
    }

    void finishApply(bool success, bool refreshClients, const QString &message)
    {
        if (!applyProcess_)
            return;
        applyProcess_->deleteLater();
        applyProcess_ = nullptr;
        const QString detail = message.isEmpty()
            ? (success ? QStringLiteral("Configuration applied")
                       : QStringLiteral("Authorization was cancelled or the helper failed."))
            : message;
        setBusy(false);
        if (success) {
            if (loadProfile())
                detailLabel_->setText(QStringLiteral("%1\nPipeWire services left running.\n%2")
                                          .arg(detail, profilePath_));
        } else {
            refreshProfilePreservingEdits();
            const QString failure = refreshClients
                ? detail + QStringLiteral("\nThe daemon may have restarted. Reconnect affected clients if needed; PipeWire services were left running.")
                : detail;
            setStatus(QStringLiteral("Apply failed"), "error");
            detailLabel_->setText(failure);
            QMessageBox::critical(this, QStringLiteral("Could not apply configuration"), failure);
        }
    }

    QString profilePath_;
    QString socketPath_;
    QString helperPath_;
    QString revision_;
    QStringList loadedAssignments_;
    bool profileLoaded_ = false;
    bool restartRequired_ = false;
    bool loadingWidgets_ = false;
    bool syncingSimple_ = false;
    bool isSycon_ = false;
    bool editsPending_ = false;
    bool busy_ = false;
    QProcess *applyProcess_ = nullptr;

    QLabel *subtitleLabel_ = nullptr;
    QLabel *statusBadge_ = nullptr;
    QLabel *detailLabel_ = nullptr;
    QFrame *simpleCard_ = nullptr;
    QComboBox *simpleBufferSize_ = nullptr;
    QCheckBox *safeMode_ = nullptr;
    QLabel *modeHint_ = nullptr;
    QCheckBox *advancedToggle_ = nullptr;
    QTabWidget *settingsTabs_ = nullptr;
    QComboBox *rate_ = nullptr;
    QSpinBox *periodSize_ = nullptr;
    QSpinBox *hardwarePeriodSize_ = nullptr;
    QSpinBox *bufferSize_ = nullptr;
    QSpinBox *sharedBufferSize_ = nullptr;
    QSpinBox *playbackQueuePeriods_ = nullptr;
    QCheckBox *playbackTimerScheduling_ = nullptr;
    QComboBox *duplexLink_ = nullptr;
    QSpinBox *linkedGuardFrames_ = nullptr;
    QSpinBox *linkedPhaseAttempts_ = nullptr;
    QSpinBox *proLatencyPeriods_ = nullptr;
    QSpinBox *proHandoffUs_ = nullptr;
    QCheckBox *proHandoffAuto_ = nullptr;
    QSpinBox *proRealtimePriority_ = nullptr;
    QSpinBox *sharedLatencyPeriods_ = nullptr;
    QCheckBox *sharedPlaybackRepeatOnUnderrun_ = nullptr;
    QCheckBox *realtime_ = nullptr;
    QSpinBox *realtimePriority_ = nullptr;
    QPushButton *reloadButton_ = nullptr;
    QPushButton *applyButton_ = nullptr;
};

} // namespace

int main(int argc, char **argv)
{
    QApplication application(argc, argv);
    QCoreApplication::setApplicationName(QStringLiteral("SideALSA Control"));
    QCoreApplication::setApplicationVersion(QStringLiteral("0.1.0"));
    QCoreApplication::setOrganizationName(QStringLiteral("SideALSA"));
    QGuiApplication::setDesktopFileName(QStringLiteral("org.sidealsa.Control"));
    QApplication::setWindowIcon(QIcon(QStringLiteral(":/icons/sidealsa-icon.png")));

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("SideALSA hardware timing control panel"));
    parser.addHelpOption();
    parser.addVersionOption();
    QCommandLineOption profileOption(QStringLiteral("profile"), QStringLiteral("Installed profile path"),
                                     QStringLiteral("path"));
    QCommandLineOption socketOption(QStringLiteral("socket"), QStringLiteral("Daemon socket path"),
                                    QStringLiteral("path"));
    parser.addOption(profileOption);
    parser.addOption(socketOption);
    parser.process(application);

    ControlWindow window(parser.value(profileOption), parser.value(socketOption));
    window.show();
    return application.exec();
}
