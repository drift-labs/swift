// Kafka helper cli
import { execSync } from 'child_process';
import { writeFileSync, unlinkSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { Command } from 'commander';

const program = new Command();

const broker = process.env.BROKER_STRING;
if (!broker) {
  console.error("Error: BROKER_STRING environment variable not set.");
  process.exit(1);
}

const kafkaBin = process.env.KAFKA_BIN || '/opt/kafka/bin';
const clientConfigArg = process.env.KAFKA_CLIENT_CONFIG ? ` --command-config ${process.env.KAFKA_CLIENT_CONFIG}` : '';

// Helper function to run shell commands
function runCommand(cmd: string, description: string) {
  try {
    console.log(`Running: ${cmd}`);
    execSync(cmd, { stdio: 'inherit' });
  } catch (error) {
    console.error(`Error during ${description}.`);
    process.exit(1);
  }
}

program
  .command('rebalance')
  .description('Rebalance partition assignments evenly across available brokers using partition reassignment.')
  .action(() => {
    // Step 1: Describe all topics
    console.log("Describing topics...");
    const describeCmd = `${kafkaBin}/kafka-topics.sh --bootstrap-server ${broker}${clientConfigArg} --describe`;
    let describeOutput: string;
    try {
      describeOutput = execSync(describeCmd, { encoding: 'utf-8' });
    } catch (error) {
      console.error("Error describing topics.");
      process.exit(1);
    }

    const lines = describeOutput.split('\n').filter(line => line.trim().length > 0);

    type PartitionInfo = { topic: string, partition: number, currentReplicas: number[] };
    const partitionInfos: PartitionInfo[] = [];
    const brokerSet = new Set<number>();

    const regex = /Topic:\s*(\S+)\s+Partition:\s*(\d+).*Replicas:\s*([\d,]+)/;

    for (const line of lines) {
      const match = line.match(regex);
      if (match) {
        const topic = match[1];
        if (topic.startsWith('__')) continue;
        const partition = parseInt(match[2]);
        const replicasStr = match[3];
        const currentReplicas = replicasStr.split(',').map(s => parseInt(s.trim()));
        partitionInfos.push({ topic, partition, currentReplicas });
        currentReplicas.forEach(r => brokerSet.add(r));
      }
    }

    const brokers = Array.from(brokerSet).sort((a, b) => a - b);
    if (brokers.length === 0) {
      console.error("No brokers found from topic descriptions.");
      process.exit(1);
    }
    console.log("Brokers found:", brokers);

    
    const newPartitions = partitionInfos.map(pi => {
      const replicationFactor = pi.currentReplicas.length;
      const newReplicas: number[] = [];
      for (let i = 0; i < replicationFactor; i++) {
        const brokerId = brokers[(pi.partition + i) % brokers.length];
        newReplicas.push(brokerId);
      }
      return { topic: pi.topic, partition: pi.partition, replicas: newReplicas };
    });

    const reassignmentPlan = {
      version: 1,
      partitions: newPartitions
    };

    const planFile = join(tmpdir(), `plan-${Date.now()}.json`);
    writeFileSync(planFile, JSON.stringify(reassignmentPlan, null, 2));
    console.log(`Reassignment plan written to ${planFile}`);

    const executeCmd = `${kafkaBin}/kafka-reassign-partitions.sh --bootstrap-server ${broker}${clientConfigArg} --execute --reassignment-json-file ${planFile}`;
    runCommand(executeCmd, 'executing reassignment plan');

    const verifyCmd = `${kafkaBin}/kafka-reassign-partitions.sh --bootstrap-server ${broker}${clientConfigArg} --verify --reassignment-json-file ${planFile}`;
    runCommand(verifyCmd, 'verifying reassignment plan');

    // Clean up the temporary JSON file.
    unlinkSync(planFile);
  });

program
  .command('add-partitions <newCount> <topics...>')
  .description('Add partitions to specified topics and rebalance.')
  .action((newCount: string, topics: string[]) => {
    topics.forEach(topic => {
      console.log(`Altering topic '${topic}' to have ${newCount} partitions.`);
      const alterCmd = `${kafkaBin}/kafka-topics.sh --bootstrap-server ${broker} --alter --topic ${topic} --partitions ${newCount}${clientConfigArg}`;
      runCommand(alterCmd, `altering topic ${topic}`);
    });

    // Create a temporary JSON file for reassignment
    const topicsJson = {
      topics: topics.map(topic => ({ topic })),
      version: 1
    };
    const tempFile = join(tmpdir(), `topics-${Date.now()}.json`);
    writeFileSync(tempFile, JSON.stringify(topicsJson));
    console.log(`Temporary topics file created: ${tempFile}`);

    // Generate a reassignment plan
    const planFile = join(tmpdir(), `plan-${Date.now()}.json`);
    const generateCmd = `${kafkaBin}/kafka-reassign-partitions.sh --bootstrap-server ${broker} --generate --topics-to-move-json-file ${tempFile} --broker-list ${broker}${clientConfigArg}`;
    try {
      console.log("Generating reassignment plan...");
      const planOutput = execSync(generateCmd, { encoding: 'utf-8' });
      writeFileSync(planFile, planOutput);
      console.log(`Reassignment plan saved to: ${planFile}`);
    } catch (error) {
      console.error("Error generating reassignment plan.");
      process.exit(1);
    }

    // Execute the reassignment plan
    const executeCmd = `${kafkaBin}/kafka-reassign-partitions.sh --bootstrap-server ${broker} --execute --reassignment-json-file ${planFile}${clientConfigArg}`;
    runCommand(executeCmd, 'executing reassignment plan');

    // Verify the reassignment plan
    const verifyCmd = `${kafkaBin}/kafka-reassign-partitions.sh --bootstrap-server ${broker} --verify --reassignment-json-file ${planFile}${clientConfigArg}`;
    runCommand(verifyCmd, 'verifying reassignment plan');

    // Clean up temporary files
    unlinkSync(tempFile);
    unlinkSync(planFile);
  });

program
  .command('offset-reset')
  .description('Reset consumer group offsets to the latest available offset so that consumers start from now.')
  .action(() => {
    console.log("Listing consumer groups for offset reset to latest...");
    const listGroupsCmd = `${kafkaBin}/kafka-consumer-groups.sh --bootstrap-server ${broker} --list${clientConfigArg}`;
    let groupsOutput: string;
    try {
      groupsOutput = execSync(listGroupsCmd, { encoding: 'utf-8' });
    } catch (error) {
      console.error("Error listing consumer groups.");
      process.exit(1);
    }
    // Reset offsets for all groups found (adjust filtering if you need to target only specific groups)
    const groups = groupsOutput
      .split('\n')
      .map(g => g.trim())
      .filter(g => g && g.length > 0);

    if (groups.length === 0) {
      console.log("No consumer groups found.");
      return;
    }
    groups.forEach(group => {
      console.log(`Resetting offsets for consumer group: ${group} to latest`);
      const resetCmd = `${kafkaBin}/kafka-consumer-groups.sh --bootstrap-server ${broker} --group ${group} --reset-offsets --to-latest --execute --all-topics${clientConfigArg}`;
      runCommand(resetCmd, `resetting offsets to latest for group ${group}`);
    });
  });

program.parse(process.argv);
