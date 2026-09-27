import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

export function Panel(props: {
  title: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{props.title}</CardTitle>
        {props.hint && <CardDescription>{props.hint}</CardDescription>}
      </CardHeader>
      <CardContent>{props.children}</CardContent>
    </Card>
  );
}
