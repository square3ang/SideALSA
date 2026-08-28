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
#include <QLabel>
#include <QLineEdit>
#include <QMainWindow>
#include <QMessageBox>
#include <QPointer>
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

#include <limits>

namespace {

constexpr auto kDefaultProfile = "/etc/sidealsa/profiles/topping-e1x2.toml";
constexpr auto kDefaultSocket = "/tmp/sidealsad.sock";
constexpr int kClientRefreshRequiredErrorExitCode = 2;
constexpr int kAudioRestartTimeoutMs = 30000;

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
    box->setButtonSymbols(QAbstractSpinBox::NoButtons);
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
        resize(860, 700);
        setMinimumSize(680, 580);
        buildUi();
        loadProfile();
    }

protected:
    void closeEvent(QCloseEvent *event) override
    {
        if (applyProcess_ || audioRestartProcess_) {
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

        auto *eyebrow = new QLabel(QStringLiteral("PROFESSIONAL AUDIO ENGINE"));
        eyebrow->setObjectName(QStringLiteral("eyebrow"));
        auto *title = new QLabel(QStringLiteral("SideALSA Control"));
        title->setObjectName(QStringLiteral("title"));
        auto *subtitle = new QLabel(
            QStringLiteral("Tune the hardware timeline. Changes are validated, persisted, and applied immediately."));
        subtitle->setObjectName(QStringLiteral("subtitle"));
        subtitle->setWordWrap(true);
        statusBadge_ = new QLabel(QStringLiteral("Checking daemon"));
        statusBadge_->setObjectName(QStringLiteral("statusBadge"));
        statusBadge_->setProperty("state", "idle");
        statusBadge_->setAlignment(Qt::AlignCenter);
        statusBadge_->setMinimumWidth(138);

        heroLayout->addWidget(eyebrow, 0, 0);
        heroLayout->addWidget(statusBadge_, 0, 1, 2, 1, Qt::AlignRight | Qt::AlignTop);
        heroLayout->addWidget(title, 1, 0);
        heroLayout->addWidget(subtitle, 2, 0, 1, 2);
        root->addWidget(hero);

        settingsTabs_ = new QTabWidget;
        settingsTabs_->setObjectName(QStringLiteral("settingsTabs"));
        settingsTabs_->setDocumentMode(true);

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
        rate_->setToolTip(QStringLiteral("Physical playback and capture sample rate"));

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
        clockForm->addRow(QStringLiteral("Sample rate"), rate_);
        clockForm->addRow(QStringLiteral("Client period"), periodSize_);
        clockForm->addRow(QStringLiteral("Physical period"), hardwarePeriodSize_);
        clockForm->addRow(QStringLiteral("Hardware buffer"), bufferSize_);
        clockForm->addRow(QStringLiteral("Shared buffer"), sharedBufferSize_);
        clockForm->addRow(QStringLiteral("Playback queue"), playbackQueuePeriods_);
        settingsTabs_->addTab(
            settingsPage(
                QStringLiteral("Clock & buffers"),
                QStringLiteral("The sample rate and ALSA queue geometry. Auto uses the profile-derived value."),
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
        proHandoffUs_ = numberBox(1, maximumInteger, QStringLiteral(" us"));
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
        schedulingForm->addRow(QStringLiteral("PRO handoff"), proHandoffUs_);
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

        const auto watchSpinBox = [this](QSpinBox *box) {
            connect(box, qOverload<int>(&QSpinBox::valueChanged), this,
                    [this] { markEdited(); });
        };
        for (QSpinBox *box : {periodSize_, hardwarePeriodSize_, bufferSize_, sharedBufferSize_,
                              playbackQueuePeriods_, linkedGuardFrames_, linkedPhaseAttempts_,
                              proLatencyPeriods_, proHandoffUs_, proRealtimePriority_,
                              sharedLatencyPeriods_, realtimePriority_}) {
            watchSpinBox(box);
        }
        connect(rate_, &QComboBox::currentTextChanged, this,
                [this] { markEdited(); });
        connect(duplexLink_, qOverload<int>(&QComboBox::currentIndexChanged), this,
                [this] { markEdited(); });
        connect(playbackTimerScheduling_, &QCheckBox::toggled, this,
                [this] { markEdited(); });
        connect(sharedPlaybackRepeatOnUnderrun_, &QCheckBox::toggled, this,
                [this] { markEdited(); });
        connect(realtime_, &QCheckBox::toggled, this, [this] { markEdited(); });

        setCentralWidget(central);
        setStyleSheet(QStringLiteral(R"(
            QMainWindow { background: palette(window); }
            QFrame#hero {
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
        settingsTabs_->setEnabled(!busy);
        updateApplyState();
        if (busy)
            setStatus(QStringLiteral("Applying"), "warning");
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
        process.start(helperPath_, {QStringLiteral("show"), QStringLiteral("--profile"), profilePath_,
                                    QStringLiteral("--socket"), socketPath_});
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
        if (revision_.isEmpty()) {
            setStatus(QStringLiteral("Invalid helper output"), "error");
            updateApplyState();
            return false;
        }
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
            detailLabel_->setText(
                QStringLiteral("PID %1  |  %2 Hz  |  Q%3/%4  |  B%5\n%6")
                    .arg(values.value(QStringLiteral("daemon_pid")),
                         values.value(QStringLiteral("daemon_rate")),
                         values.value(QStringLiteral("daemon_period_size")),
                         values.value(QStringLiteral("daemon_hardware_period_size")),
                         values.value(QStringLiteral("daemon_buffer_size")), profilePath_));
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
        valid &= setOptional(proRealtimePriority_,
                             values.value(QStringLiteral("pro_realtime_priority")));
        valid &= setNumber(sharedLatencyPeriods_,
                           values.value(QStringLiteral("shared_latency_periods")));
        sharedPlaybackRepeatOnUnderrun_->setChecked(
            textBool(values.value(QStringLiteral("shared_playback_repeat_on_underrun"))));
        realtime_->setChecked(textBool(values.value(QStringLiteral("realtime"))));
        valid &= setNumber(realtimePriority_, values.value(QStringLiteral("realtime_priority")));
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
            QStringLiteral("SideALSA will restart the physical stream and user PipeWire services. "
                           "Direct PRO and SHARED clients will disconnect, and desktop audio will pause briefly.\n\n"
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
                            "The apply helper terminated unexpectedly. If sidealsad restarted, run: "
                            "systemctl --user restart pipewire.service pipewire-pulse.service "
                            "wireplumber.service");
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
        if (refreshClients) {
            pendingApplySucceeded_ = success;
            pendingApplyMessage_ = detail;
            restartUserAudio();
            return;
        }
        setBusy(false);
        refreshProfilePreservingEdits();
        setStatus(QStringLiteral("Apply failed"), "error");
        detailLabel_->setText(detail);
        QMessageBox::critical(this, QStringLiteral("Could not apply configuration"), detail);
    }

    void restartUserAudio()
    {
        detailLabel_->setText(
            pendingApplySucceeded_
                ? QStringLiteral("Configuration applied. Restarting user PipeWire services...")
                : QStringLiteral("Apply failed. Refreshing user PipeWire connections..."));
        audioRestartTimedOut_ = false;
        audioRestartProcess_ = new QProcess(this);
        connect(audioRestartProcess_, &QProcess::errorOccurred, this,
                [this](QProcess::ProcessError error) {
                    if (error == QProcess::FailedToStart)
                        finishAudioRestart(false, QStringLiteral("Could not start systemctl."));
                });
        connect(audioRestartProcess_, qOverload<int, QProcess::ExitStatus>(&QProcess::finished), this,
                [this](int exitCode, QProcess::ExitStatus exitStatus) {
                    if (!audioRestartProcess_)
                        return;
                    QString standardError =
                        QString::fromUtf8(audioRestartProcess_->readAllStandardError()).trimmed();
                    const bool processSucceeded =
                        exitStatus == QProcess::NormalExit && exitCode == 0;
                    if (audioRestartTimedOut_ && !processSucceeded) {
                        const QString timeout = QStringLiteral(
                            "systemctl did not finish within 30 seconds; user-service state is indeterminate");
                        standardError = standardError.isEmpty()
                            ? timeout
                            : QStringLiteral("%1; %2").arg(standardError, timeout);
                    }
                    const bool success = processSucceeded;
                    finishAudioRestart(success, standardError);
                });
        const QPointer<QProcess> process(audioRestartProcess_);
        QTimer::singleShot(kAudioRestartTimeoutMs, this, [this, process] {
            if (process && audioRestartProcess_ == process
                && process->state() != QProcess::NotRunning) {
                audioRestartTimedOut_ = true;
                process->kill();
            }
        });
        audioRestartProcess_->start(
            QStringLiteral("/usr/bin/systemctl"),
            {QStringLiteral("--user"), QStringLiteral("try-restart"),
             QStringLiteral("pipewire.service"), QStringLiteral("pipewire-pulse.service"),
             QStringLiteral("wireplumber.service")});
    }

    void finishAudioRestart(bool success, const QString &error)
    {
        if (!audioRestartProcess_)
            return;
        audioRestartProcess_->deleteLater();
        audioRestartProcess_ = nullptr;
        setBusy(false);
        const bool daemonReady = pendingApplySucceeded_ ? loadProfile()
                                                        : refreshProfilePreservingEdits();
        QString clientRestartFailure;
        if (!success) {
            const QString reason = error.isEmpty() ? QStringLiteral("systemctl returned an error") : error;
            clientRestartFailure =
                QStringLiteral("PipeWire service refresh did not complete cleanly: %1\n"
                               "Run: systemctl --user restart pipewire.service pipewire-pulse.service "
                               "wireplumber.service")
                    .arg(reason);
        }

        if (pendingApplySucceeded_) {
            if (daemonReady && success) {
                detailLabel_->setText(QStringLiteral("%1\nActive PipeWire services refreshed.\n%2")
                                          .arg(pendingApplyMessage_, profilePath_));
            } else if (daemonReady) {
                setStatus(QStringLiteral("Client restart needed"), "warning");
                detailLabel_->setText(QStringLiteral("%1\n%2")
                                          .arg(pendingApplyMessage_, clientRestartFailure));
            } else if (!success) {
                detailLabel_->setText(QStringLiteral("%1\n\n%2")
                                          .arg(detailLabel_->text(), clientRestartFailure));
            }
        } else {
            QString detail = pendingApplyMessage_;
            if (!success)
                detail += QStringLiteral("\n") + clientRestartFailure;
            setStatus(QStringLiteral("Apply failed"), "error");
            detailLabel_->setText(detail);
            QMessageBox::critical(this, QStringLiteral("Could not apply configuration"), detail);
        }
        pendingApplyMessage_.clear();
        pendingApplySucceeded_ = false;
        audioRestartTimedOut_ = false;
    }

    QString profilePath_;
    QString socketPath_;
    QString helperPath_;
    QString revision_;
    QStringList loadedAssignments_;
    QString pendingApplyMessage_;
    bool profileLoaded_ = false;
    bool restartRequired_ = false;
    bool loadingWidgets_ = false;
    bool editsPending_ = false;
    bool busy_ = false;
    bool pendingApplySucceeded_ = false;
    QProcess *applyProcess_ = nullptr;
    QProcess *audioRestartProcess_ = nullptr;
    bool audioRestartTimedOut_ = false;

    QLabel *statusBadge_ = nullptr;
    QLabel *detailLabel_ = nullptr;
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

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("SideALSA hardware timing control panel"));
    parser.addHelpOption();
    parser.addVersionOption();
    QCommandLineOption profileOption(QStringLiteral("profile"), QStringLiteral("Installed profile path"),
                                     QStringLiteral("path"), QString::fromUtf8(kDefaultProfile));
    QCommandLineOption socketOption(QStringLiteral("socket"), QStringLiteral("Daemon socket path"),
                                    QStringLiteral("path"), QString::fromUtf8(kDefaultSocket));
    parser.addOption(profileOption);
    parser.addOption(socketOption);
    parser.process(application);

    ControlWindow window(parser.value(profileOption), parser.value(socketOption));
    window.show();
    return application.exec();
}
