#include <QApplication>
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
#include <QTimer>
#include <QVBoxLayout>
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

QFrame *card(const QString &title, const QString &description, QLayout *content)
{
    auto *frame = new QFrame;
    frame->setObjectName(QStringLiteral("card"));
    auto *layout = new QVBoxLayout(frame);
    layout->setContentsMargins(22, 18, 22, 20);
    layout->setSpacing(8);

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
    return frame;
}

QSpinBox *numberBox(int minimum, int maximum, const QString &suffix = {})
{
    auto *box = new QSpinBox;
    box->setRange(minimum, maximum);
    box->setAccelerated(true);
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
        resize(790, 780);
        setMinimumSize(650, 620);
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
        event->accept();
    }

private:
    void buildUi()
    {
        auto *scroll = new QScrollArea;
        scroll->setWidgetResizable(true);
        scroll->setFrameShape(QFrame::NoFrame);
        auto *page = new QWidget;
        auto *root = new QVBoxLayout(page);
        root->setContentsMargins(28, 26, 28, 28);
        root->setSpacing(16);

        auto *hero = new QFrame;
        hero->setObjectName(QStringLiteral("hero"));
        auto *heroLayout = new QGridLayout(hero);
        heroLayout->setContentsMargins(24, 21, 24, 21);
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

        rate_ = new QComboBox;
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
        root->addWidget(card(
            QStringLiteral("Clock & buffers"),
            QStringLiteral("The sample rate and ALSA queue geometry. Auto uses the profile-derived value."),
            clockForm));

        playbackTimerScheduling_ = new QCheckBox(QStringLiteral("Use timer-driven playback scheduling"));
        duplexLink_ = new QComboBox;
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
        root->addWidget(card(
            QStringLiteral("Duplex timeline"),
            QStringLiteral("Controls linked capture/playback scheduling and zero-lead write safety."),
            duplexForm));

        proLatencyPeriods_ = numberBox(0, 7, QStringLiteral(" periods"));
        proHandoffUs_ = numberBox(1, maximumInteger, QStringLiteral(" us"));
        proRealtimePriority_ = optionalNumberBox(99);
        sharedLatencyPeriods_ = numberBox(0, 7, QStringLiteral(" periods"));
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
        schedulingForm->addRow(QString(), realtime_);
        schedulingForm->addRow(QStringLiteral("Hardware RT priority"), realtimePriority_);
        root->addWidget(card(
            QStringLiteral("Scheduling"),
            QStringLiteral("Deadline budget and thread priorities. Invalid combinations are rejected before restart."),
            schedulingForm));

        auto *footer = new QFrame;
        footer->setObjectName(QStringLiteral("footer"));
        auto *footerLayout = new QGridLayout(footer);
        footerLayout->setContentsMargins(4, 4, 4, 4);
        footerLayout->setColumnStretch(0, 1);
        detailLabel_ = new QLabel;
        detailLabel_->setObjectName(QStringLiteral("detail"));
        detailLabel_->setWordWrap(true);
        reloadButton_ = new QPushButton(QStringLiteral("Reload"));
        applyButton_ = new QPushButton(QStringLiteral("Apply configuration"));
        applyButton_->setObjectName(QStringLiteral("primaryButton"));
        applyButton_->setDefault(true);
        footerLayout->addWidget(detailLabel_, 0, 0, 1, 3);
        footerLayout->addWidget(reloadButton_, 1, 1);
        footerLayout->addWidget(applyButton_, 1, 2);
        root->addWidget(footer);

        connect(reloadButton_, &QPushButton::clicked, this, [this] { loadProfile(); });
        connect(applyButton_, &QPushButton::clicked, this, [this] { applyProfile(); });

        scroll->setWidget(page);
        setCentralWidget(scroll);
        setStyleSheet(QStringLiteral(R"(
            QMainWindow { background: palette(window); }
            QFrame#hero, QFrame#card {
                background: palette(base);
                border: 1px solid palette(mid);
                border-radius: 12px;
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
                min-height: 30px;
                padding-left: 7px;
            }
            QPushButton { min-height: 32px; padding: 0 15px; }
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
        applyButton_->setDisabled(busy);
        reloadButton_->setDisabled(busy);
        if (busy)
            setStatus(QStringLiteral("Applying"), "warning");
    }

    bool loadProfile()
    {
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
            applyButton_->setDisabled(true);
            return false;
        }

        const QHash<QString, QString> values = parseSettings(process.readAllStandardOutput());
        revision_ = values.value(QStringLiteral("revision"));
        if (revision_.isEmpty()) {
            setStatus(QStringLiteral("Invalid helper output"), "error");
            applyButton_->setDisabled(true);
            return false;
        }
        if (!loadWidgets(values)) {
            setStatus(QStringLiteral("Unsupported profile"), "error");
            detailLabel_->setText(
                QStringLiteral("A timing value is missing or exceeds this control panel's integer range.\n%1")
                    .arg(profilePath_));
            applyButton_->setDisabled(true);
            return false;
        }
        applyButton_->setEnabled(true);

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
            setStatus(QStringLiteral("Restart required"), "warning");
            detailLabel_->setText(QStringLiteral("The saved profile differs from the running daemon.\n%1")
                                      .arg(profilePath_));
            return false;
        } else {
            setStatus(QStringLiteral("Daemon offline"), "error");
            detailLabel_->setText(QStringLiteral("%1\n%2")
                                      .arg(values.value(QStringLiteral("daemon_error")), profilePath_));
            return false;
        }
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
        if (loadProfile()) {
            setStatus(QStringLiteral("Apply failed"), "error");
            detailLabel_->setText(detail);
        } else {
            detailLabel_->setText(QStringLiteral("%1\n\nApply failed: %2")
                                      .arg(detailLabel_->text(), detail));
        }
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
                    if (audioRestartTimedOut_) {
                        const QString timeout = QStringLiteral(
                            "systemctl did not finish within 30 seconds; user-service state is indeterminate");
                        standardError = standardError.isEmpty()
                            ? timeout
                            : QStringLiteral("%1; %2").arg(standardError, timeout);
                    }
                    const bool success = !audioRestartTimedOut_
                        && exitStatus == QProcess::NormalExit && exitCode == 0;
                    finishAudioRestart(success, standardError);
                });
        const QPointer<QProcess> process(audioRestartProcess_);
        QTimer::singleShot(kAudioRestartTimeoutMs, this, [this, process] {
            if (process && audioRestartProcess_ == process) {
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
        const bool daemonReady = loadProfile();
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
            if (daemonReady) {
                setStatus(QStringLiteral("Apply failed"), "error");
                detailLabel_->setText(detail);
            } else {
                detailLabel_->setText(QStringLiteral("%1\n\nApply failed: %2")
                                          .arg(detailLabel_->text(), detail));
            }
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
    QString pendingApplyMessage_;
    bool pendingApplySucceeded_ = false;
    QProcess *applyProcess_ = nullptr;
    QProcess *audioRestartProcess_ = nullptr;
    bool audioRestartTimedOut_ = false;

    QLabel *statusBadge_ = nullptr;
    QLabel *detailLabel_ = nullptr;
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
